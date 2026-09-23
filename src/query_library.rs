use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const START: &str = "// <dataexplorer-query>";
const END: &str = "// </dataexplorer-query>";
pub const MAX_QUERY_BYTES: usize = 4 * 1024 * 1024;
const MAX_LIBRARY_BYTES: usize = 64 * 1024 * 1024;
pub type Values = BTreeMap<String, String>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub description: String,
    pub default: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Documentation {
    pub description: String,
    pub parameters: Vec<Parameter>,
}

pub fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Parameter {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            identifier(&self.name),
            "parameter name must be an ASCII identifier"
        );
        ensure!(
            matches!(
                self.kind.as_str(),
                "string"
                    | "bool"
                    | "int"
                    | "long"
                    | "real"
                    | "decimal"
                    | "datetime"
                    | "timespan"
                    | "guid"
                    | "dynamic"
            ),
            "unsupported parameter type {}; use string, bool, int, long, real, decimal, datetime, timespan, guid, dynamic",
            self.kind
        );
        if let Some(default) = &self.default {
            ensure!(
                self.kind != "dynamic",
                "Kusto dynamic parameters cannot have defaults"
            );
            self.literal(default)?;
        }
        Ok(())
    }

    // Inputs are values, never arbitrary KQL expressions; only generated literals enter declarations.
    pub fn literal(&self, value: &str) -> Result<String> {
        let v = value.trim();
        Ok(match self.kind.as_str() {
            "string" => serde_json::to_string(value)?,
            "bool" => {
                ensure!(matches!(v, "true" | "false"), "bool must be true or false");
                v.into()
            }
            "int" => v
                .parse::<i32>()
                .context("int must be a 32-bit integer")?
                .to_string(),
            "long" => format!(
                "long({})",
                v.parse::<i64>().context("long must be a 64-bit integer")?
            ),
            "real" => {
                let n = v.parse::<f64>().context("real must be a number")?;
                ensure!(n.is_finite(), "real must be finite");
                format!("real({n})")
            }
            "decimal" => {
                let n = v
                    .parse::<bigdecimal::BigDecimal>()
                    .context("invalid decimal")?;
                format!("decimal({n})")
            }
            "datetime" => {
                let date = chrono::DateTime::parse_from_rfc3339(v)
                    .context("datetime must be RFC3339, e.g. 2026-01-01T00:00:00Z")?;
                format!("datetime({})", date.to_rfc3339())
            }
            "timespan" => {
                // Canonical constant format avoids embedding arbitrary expressions.
                let (sign, unsigned) = v.strip_prefix('-').map_or(("", v), |s| ("-", s));
                let parts: Vec<_> = unsigned.split(':').collect();
                ensure!(
                    parts.len() == 3,
                    "timespan must be [-][days.]hh:mm:ss[.fraction]"
                );
                let (days, hours) = parts[0].split_once('.').unwrap_or(("0", parts[0]));
                let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
                let (seconds, fraction) = parts[2].split_once('.').unwrap_or((parts[2], ""));
                ensure!(
                    digits(days)
                        && digits(hours)
                        && digits(parts[1])
                        && digits(seconds)
                        && fraction.bytes().all(|c| c.is_ascii_digit())
                        && fraction.len() <= 7,
                    "invalid timespan"
                );
                let d = days.parse::<u32>().context("timespan days too large")?;
                let h = hours.parse::<u32>()?;
                let m = parts[1].parse::<u32>()?;
                let s = seconds.parse::<u32>()?;
                ensure!(h < 24 && m < 60 && s < 60, "invalid timespan clock fields");
                format!(
                    "timespan({sign}{d}.{h:02}:{m:02}:{s:02}{})",
                    if fraction.is_empty() {
                        String::new()
                    } else {
                        format!(".{fraction}")
                    }
                )
            }
            "guid" => format!(
                "guid({})",
                uuid::Uuid::parse_str(v).context("invalid guid")?
            ),
            "dynamic" => {
                let data: serde_json::Value =
                    serde_json::from_str(value).context("dynamic value must be JSON")?;
                format!("dynamic({data})")
            }
            _ => bail!("unsupported parameter type {}", self.kind),
        })
    }
}

impl Documentation {
    pub fn validate(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for parameter in &self.parameters {
            parameter
                .validate()
                .with_context(|| format!("parameter {}", parameter.name))?;
            ensure!(
                names.insert(&parameter.name),
                "duplicate parameter {}",
                parameter.name
            );
        }
        Ok(())
    }
    fn declaration(&self) -> Result<String> {
        if self.parameters.is_empty() {
            return Ok(String::new());
        }
        let declarations = self
            .parameters
            .iter()
            .map(|p| {
                let default = p
                    .default
                    .as_ref()
                    .map(|v| p.literal(v))
                    .transpose()?
                    .map(|v| format!(" = {v}"))
                    .unwrap_or_default();
                // Bracket escaping also makes reserved words legal parameter names.
                Ok(format!("['{}']:{}{default}", p.name, p.kind))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!(
            "declare query_parameters({});",
            declarations.join(", ")
        ))
    }
    pub fn request_values(&self, values: &Values) -> Result<Values> {
        self.validate()?;
        for name in values.keys() {
            ensure!(
                self.parameters.iter().any(|p| &p.name == name),
                "value for undefined parameter {name}"
            );
        }
        self.parameters
            .iter()
            .filter_map(|p| {
                match values.get(&p.name) {
                    Some(value) => Some(
                        p.literal(value)
                            .map(|literal| {
                                // Kusto string request parameters are raw strings, not quoted KQL source.
                                (
                                    p.name.clone(),
                                    if p.kind == "string" {
                                        value.clone()
                                    } else {
                                        literal
                                    },
                                )
                            })
                            .with_context(|| format!("parameter {}", p.name)),
                    ),
                    None if p.default.is_some() => None,
                    None => Some(Err(anyhow::anyhow!(
                        "parameter {} requires a value; open F4 Parameters",
                        p.name
                    ))),
                }
            })
            .collect()
    }
    pub fn preview(&self) -> String {
        let mut text = self.description.clone();
        text.push_str("\n\nParameters:\n");
        if self.parameters.is_empty() {
            text.push_str("(none documented)");
        }
        for p in &self.parameters {
            text.push_str(&format!(
                "\n{} : {}\n{}\nDefault: {}\n",
                p.name,
                p.kind,
                p.description,
                p.default.as_deref().unwrap_or("(required)")
            ));
        }
        text
    }
}

pub fn parse(text: &str) -> Result<(Documentation, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if !text.starts_with(START) {
        let description = text
            .lines()
            .take_while(|line| line.trim_start().starts_with("//"))
            .map(|line| line.trim_start().trim_start_matches('/').trim_start())
            .collect::<Vec<_>>()
            .join("\n");
        return Ok((
            Documentation {
                description,
                ..Default::default()
            },
            text.into(),
        ));
    }
    let mut doc = Documentation::default();
    let mut description = Vec::new();
    let mut declaration = Vec::new();
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        offset += line.len();
        let line = line.trim_end_matches(['\r', '\n']);
        if index == 0 {
            ensure!(line == START, "invalid query documentation header");
            continue;
        }
        if line == END {
            doc.description = description.join("\n");
            doc.validate()?;
            ensure!(
                declaration.join("\n") == doc.declaration()?,
                "managed parameter declaration was edited; keep the documentation and declaration consistent (F4 edits both)"
            );
            return Ok((doc, text[offset..].to_owned()));
        }
        if let Some(json) = line.strip_prefix("/// @param ") {
            doc.parameters
                .push(serde_json::from_str(json).context("invalid @param documentation")?);
        } else if let Some(value) = line.strip_prefix("/// ") {
            description.push(value.to_owned());
        } else if line == "///" {
            description.push(String::new());
        } else {
            declaration.push(line.to_owned());
        }
    }
    bail!("query documentation header is missing {END}")
}

pub fn document(doc: &Documentation, body: &str) -> Result<String> {
    doc.validate()?;
    let mut text = format!("{START}\n");
    for line in doc.description.split('\n') {
        // Encode a leading @param like ordinary prose so descriptions cannot become metadata.
        ensure!(
            !line.starts_with("@param "),
            "description lines cannot start with @param"
        );
        text.push_str(&format!("/// {line}\n"));
    }
    for p in &doc.parameters {
        text.push_str(&format!("/// @param {}\n", serde_json::to_string(p)?));
    }
    let declaration = doc.declaration()?;
    if !declaration.is_empty() {
        text.push_str(&declaration);
        text.push('\n');
    }
    text.push_str(END);
    text.push('\n');
    text.push_str(body);
    ensure!(
        text.len() <= MAX_QUERY_BYTES,
        "query exceeds 4 MiB editor limit"
    );
    Ok(text)
}

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub text: String,
    pub documentation: Documentation,
}

#[derive(Default)]
pub struct Library {
    pub entries: Vec<Entry>,
    pub errors: Vec<String>,
}

pub fn read_query(path: &Path) -> Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take((MAX_QUERY_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .with_context(|| format!("reading query {}", path.display()))?;
    ensure!(
        text.len() <= MAX_QUERY_BYTES,
        "query exceeds 4 MiB: {}",
        path.display()
    );
    Ok(text)
}

pub fn scan(root: &Path) -> Result<Library> {
    let root = root
        .canonicalize()
        .with_context(|| format!("query_path {} is unavailable", root.display()))?;
    ensure!(root.is_dir(), "query_path must be a directory");
    let mut result = Library::default();
    let mut directories = vec![root.clone()];
    let mut bytes = 0;
    while let Some(directory) = directories.pop() {
        let items = match std::fs::read_dir(&directory) {
            Ok(items) => items,
            Err(e) => {
                result.errors.push(format!("{}: {e}", directory.display()));
                continue;
            }
        };
        for item in items {
            let loaded = (|| -> Result<()> {
                let item = item?;
                let kind = item.file_type()?;
                let path = item.path();
                if kind.is_symlink() {
                    result.errors.push(format!(
                        "Skipped symlink {} (library does not follow links)",
                        path.display()
                    ));
                } else if kind.is_dir() {
                    directories.push(path);
                } else if kind.is_file() && is_query(&path) {
                    ensure!(
                        result.entries.len() < 10_000,
                        "query library exceeds 10000 files"
                    );
                    let text = read_query(&path)?;
                    ensure!(
                        bytes + text.len() <= MAX_LIBRARY_BYTES,
                        "query library exceeds 64 MiB"
                    );
                    let (documentation, _) =
                        parse(&text).with_context(|| format!("query {}", path.display()))?;
                    bytes += text.len();
                    result.entries.push(Entry {
                        relative: path.strip_prefix(&root)?.to_owned(),
                        path,
                        text,
                        documentation,
                    });
                }
                Ok(())
            })();
            if let Err(e) = loaded {
                result.errors.push(format!("{e:#}"));
            }
        }
    }
    result.entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(result)
}

pub fn is_query(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        ["kql", "csl", "kusto"]
            .iter()
            .any(|ext| s.eq_ignore_ascii_case(ext))
    })
}

pub fn save_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "use a relative path within query_path, without .. or absolute paths"
    );
    ensure!(
        is_query(relative),
        "query filename must end in .kql, .csl, or .kusto"
    );
    std::fs::create_dir_all(root).context("creating query_path")?;
    let root = root.canonicalize()?;
    let mut path = root.clone();
    for component in relative.components() {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "query save path contains a symlink"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

pub fn save(root: &Path, relative: &Path, text: &str, overwrite: bool) -> Result<PathBuf> {
    ensure!(text.len() <= MAX_QUERY_BYTES, "query exceeds 4 MiB");
    parse(text)?;
    let path = save_path(root, relative)?;
    write_query(&path, text, overwrite)?;
    Ok(path)
}

pub fn explicit_save_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("query filename is required")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).context("creating query directory")?;
    let path = parent.canonicalize()?.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => ensure!(
            !meta.file_type().is_symlink(),
            "query save destination is a symlink"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(path)
}

pub fn write_query(path: &Path, text: &str, overwrite: bool) -> Result<()> {
    ensure!(text.len() <= MAX_QUERY_BYTES, "query exceeds 4 MiB");
    parse(text)?;
    crate::config::atomic_write(path, overwrite, |w| {
        w.write_all(text.as_bytes())?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc() -> Documentation {
        Documentation {
            description: "Find users\nby location".into(),
            parameters: vec![
                Parameter {
                    name: "location".into(),
                    kind: "string".into(),
                    description: "Region".into(),
                    default: Some("west\";\nprint x=1".into()),
                },
                Parameter {
                    name: "limit".into(),
                    kind: "long".into(),
                    description: "Row count".into(),
                    default: None,
                },
            ],
        }
    }
    #[test]
    fn metadata_roundtrip_defaults_and_values_are_separate() {
        let doc = doc();
        let source = document(&doc, "users | take limit").unwrap();
        let (decoded, body) = parse(&source).unwrap();
        assert_eq!(decoded, doc);
        assert_eq!(body, "users | take limit");
        assert_eq!(document(&decoded, &body).unwrap(), source);
        assert!(!source.contains("runtime-secret"));
        assert!(doc.request_values(&Values::new()).is_err());
        let values = Values::from([
            ("limit".into(), "9007199254740993".into()),
            ("location".into(), "runtime-secret".into()),
        ]);
        let wire = doc.request_values(&values).unwrap();
        assert_eq!(wire["limit"], "long(9007199254740993)");
        assert_eq!(wire["location"], "runtime-secret");
        assert!(parse(&source.replace("long);", "int);")).is_err());
        assert!(parse(START).is_err());
    }
    #[test]
    fn types_duplicates_and_invalid_defaults_are_rejected() {
        let mut d = doc();
        d.parameters.push(d.parameters[0].clone());
        assert!(d.validate().is_err());
        let mut p = d.parameters[0].clone();
        p.kind = "long".into();
        assert!(p.validate().is_err());
        p.kind = "dynamic".into();
        assert!(p.validate().is_err());
        p.default = None;
        assert!(p.literal("{\"hello\":1}").unwrap().starts_with("dynamic("));
        assert!(p.literal(");drop").is_err());
    }
    #[test]
    fn recursive_index_and_atomic_confined_save() {
        let dir = tempfile::tempdir().unwrap();
        let source = document(&doc(), "print 1").unwrap();
        let path = save(
            dir.path(),
            Path::new("team/nested/users.kql"),
            &source,
            false,
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "not a query").unwrap();
        std::fs::write(dir.path().join("broken.kql"), START).unwrap();
        let library = scan(dir.path()).unwrap();
        assert_eq!(library.entries.len(), 1);
        assert_eq!(library.entries[0].documentation, doc());
        assert_eq!(library.errors.len(), 1);
        assert!(save(dir.path(), Path::new("../outside.kql"), &source, false).is_err());
        assert!(save(dir.path(), Path::new("/absolute.kql"), &source, false).is_err());
        assert!(save(dir.path(), Path::new("team/nested/users.kql"), "new", false).is_err());
        assert_eq!(read_query(&path).unwrap(), source);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("cycle")).unwrap();
            assert!(save(dir.path(), Path::new("cycle/escape.kql"), &source, false).is_err());
            assert!(
                scan(dir.path())
                    .unwrap()
                    .errors
                    .iter()
                    .any(|e| e.contains("symlink"))
            );
        }
    }
}
