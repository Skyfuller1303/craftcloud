//! Craft app catalog and its operator-configurable text format.

/// One supported app and its native file formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDefinition {
    pub id: String,
    pub name: String,
    pub formats: Vec<String>,
}

/// Built-in Craft applications.
#[must_use]
pub fn default_app_catalog() -> Vec<AppDefinition> {
    vec![
        AppDefinition {
            id: "figcraft".into(),
            name: "FigCraft".into(),
            formats: vec!["figcraft".into(), "fig".into()],
        },
        AppDefinition {
            id: "photocraft".into(),
            name: "PhotoCraft".into(),
            formats: vec!["pcraft".into(), "psd".into()],
        },
        AppDefinition {
            id: "vectorcraft".into(),
            name: "VectorCraft".into(),
            formats: vec!["svg".into(), "ai".into()],
        },
    ]
}

/// Extends the built-in catalog with validated operator entries.
///
/// Built-ins keep their stable IDs and definitions. The extension may add
/// at most 64 entries overall and may not reuse any built-in or extension ID.
pub fn extend_app_catalog(
    mut catalog: Vec<AppDefinition>,
    contents: &str,
) -> Result<Vec<AppDefinition>, &'static str> {
    let additions = if contents
        .lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with('#'))
    {
        Vec::new()
    } else {
        parse_app_catalog(contents)?
    };
    if catalog.len().saturating_add(additions.len()) > 64 {
        return Err("at most 64 apps are allowed");
    }
    for app in additions {
        if catalog.iter().any(|existing| existing.id == app.id) {
            return Err("app id collides with a built-in or configured app");
        }
        catalog.push(app);
    }
    Ok(catalog)
}

/// Parses UTF-8 lines in `id|display name|format,format` form. Blank lines and
/// lines beginning with `#` are ignored. Invalid or duplicate IDs fail closed.
pub fn parse_app_catalog(contents: &str) -> Result<Vec<AppDefinition>, &'static str> {
    let mut apps = Vec::new();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let mut columns = line.split('|');
        let (Some(id), Some(name), Some(formats), None) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            return Err("each app line must have id|name|format,format");
        };
        let id = id.trim();
        let name = name.trim();
        if !valid_identifier(id)
            || name.is_empty()
            || name.len() > 100
            || name.chars().any(char::is_control)
            || apps.iter().any(|app: &AppDefinition| app.id == id)
        {
            return Err("app id or display name is invalid or duplicated");
        }
        let formats: Vec<String> = formats
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect();
        if formats.is_empty()
            || formats.len() > 32
            || formats.iter().any(|format| !valid_identifier(format))
        {
            return Err("file format identifiers are invalid");
        }
        apps.push(AppDefinition {
            id: id.to_owned(),
            name: name.to_owned(),
            formats,
        });
        if apps.len() > 64 {
            return Err("at most 64 apps are allowed");
        }
    }
    if apps.is_empty() {
        return Err("catalog must contain at least one app");
    }
    Ok(apps)
}

fn valid_identifier(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::{default_app_catalog, extend_app_catalog, parse_app_catalog};

    #[test]
    fn catalog_parser_rejects_ambiguous_or_duplicate_ids() {
        assert_eq!(
            parse_app_catalog("# apps\nfoo|Foo|foo,bar\n").unwrap()[0].id,
            "foo"
        );
        assert!(parse_app_catalog("foo|Foo|x\nfoo|Other|y").is_err());
        assert!(parse_app_catalog("Foo|Bad|x").is_err());
        assert!(parse_app_catalog("foo|Bad|x|extra").is_err());
    }

    #[test]
    fn catalog_file_extends_builtins_and_empty_file_preserves_them() {
        let builtins = default_app_catalog();
        let extended = extend_app_catalog(
            builtins.clone(),
            "# additional apps\nmodelcraft|ModelCraft|model,modelx\n",
        )
        .unwrap();
        assert_eq!(extended.len(), builtins.len() + 1);
        assert_eq!(extended[0], builtins[0]);
        assert_eq!(extended.last().unwrap().id, "modelcraft");
        assert_eq!(
            extend_app_catalog(builtins.clone(), "# no additions\n").unwrap(),
            builtins
        );
    }

    #[test]
    fn catalog_file_rejects_builtin_collisions_and_more_than_64_total() {
        assert!(extend_app_catalog(default_app_catalog(), "figcraft|Shadow|fig\n").is_err());

        let additions = (0..62)
            .map(|index| format!("extra{index:02}|Extra {index}|x{index:02}\n"))
            .collect::<String>();
        assert!(extend_app_catalog(default_app_catalog(), &additions).is_err());

        let additions = (0..61)
            .map(|index| format!("extra{index:02}|Extra {index}|x{index:02}\n"))
            .collect::<String>();
        let extended = extend_app_catalog(default_app_catalog(), &additions).unwrap();
        assert_eq!(extended.len(), 64);
    }
}
