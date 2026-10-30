use sha2::{Digest, Sha256};
use std::path::Path;

/// 文件类备份固定对象名
pub const FILE_OBJECT: &str = "files.tar.zst.age";
pub const MANIFEST_OBJECT: &str = "manifest.json";
/// 运行目录名与时间戳格式
pub const TIMESTAMP_FORMAT: &str = "%Y%m%d_%H%M%S";

/// 拼接 OSS key，跳过空段
pub fn join_key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// backup 名与 destination 名允许的字符
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 运行目录：`{prefix}/{destination}/{timestamp}`
pub fn run_dir(prefix: Option<&str>, destination: &str, timestamp: &str) -> String {
    join_key(&[prefix.unwrap_or(""), destination, timestamp])
}

/// 该 destination 的列举前缀：`{prefix}/{destination}/`
pub fn run_prefix(prefix: Option<&str>, destination: &str) -> String {
    format!("{}/", join_key(&[prefix.unwrap_or(""), destination]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Mysql,
    Pgsql,
}

impl DbKind {
    pub fn suffix(self) -> &'static str {
        match self {
            DbKind::Mysql => "mysql",
            DbKind::Pgsql => "pgsql",
        }
    }
}

/// 库名转对象名：非法字符替换为 `_`
pub fn db_object_name(kind: DbKind, database: &str) -> String {
    let segment = sanitize_segment(database);
    let segment = if segment.is_empty() {
        "db".to_string()
    } else {
        segment
    };
    format!("{}.{}.zst.age", segment, kind.suffix())
}

/// SQLite 用完整路径编码，避免不同目录同名文件冲突
pub fn sqlite_object_name(path: &Path) -> String {
    let base = path.with_extension("");
    let encoded: String = base
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '_',
            other => other,
        })
        .collect();
    let encoded = encoded.trim_start_matches('_').to_string();

    let name = if encoded.is_empty() {
        "sqlite".to_string()
    } else {
        sanitize_segment(&encoded)
    };

    if name.len() > 180 {
        let stem: String = name.chars().take(40).collect();
        format!(
            "{}-{}.sqlite.zst.age",
            stem,
            short_hash(path.to_string_lossy().as_ref())
        )
    } else {
        format!("{}.sqlite.zst.age", name)
    }
}

/// 保留字母数字与 `._-`，其余替换为 `_`（连续替换不折叠）
pub fn sanitize_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') || !c.is_ascii() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_key_skips_empty_parts() {
        assert_eq!(
            join_key(&["", "docs", "20260101_010101"]),
            "docs/20260101_010101"
        );
        assert_eq!(join_key(&["backups/", "/docs/"]), "backups/docs");
    }

    #[test]
    fn validates_names() {
        assert!(is_valid_name("blog-db_1.0"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("a/b"));
        assert!(!is_valid_name("中文"));
    }

    #[test]
    fn runs_dir_layout() {
        assert_eq!(
            run_dir(Some("backups"), "docs", "20260101_010101"),
            "backups/docs/20260101_010101"
        );
        assert_eq!(
            run_dir(None, "docs", "20260101_010101"),
            "docs/20260101_010101"
        );
        assert_eq!(run_prefix(Some("backups"), "docs"), "backups/docs/");
    }

    #[test]
    fn db_names() {
        assert_eq!(db_object_name(DbKind::Mysql, "blog"), "blog.mysql.zst.age");
        assert_eq!(
            db_object_name(DbKind::Pgsql, "sales.*"),
            "sales._.pgsql.zst.age"
        );
    }

    #[test]
    fn sqlite_encodes_full_path() {
        assert_eq!(
            sqlite_object_name(Path::new(r"D:\data\app.db")),
            "D__data_app.sqlite.zst.age"
        );
        assert_eq!(
            sqlite_object_name(Path::new("/var/lib/app.db")),
            "var_lib_app.sqlite.zst.age"
        );
    }

    #[test]
    fn sqlite_same_name_different_dir_differs() {
        let a = sqlite_object_name(Path::new(r"D:\a\app.db"));
        let b = sqlite_object_name(Path::new(r"D:\b\app.db"));
        assert_ne!(a, b);
    }

    #[test]
    fn sqlite_long_path_uses_hash() {
        let long = format!("D:\\{}\\app.db", "x".repeat(200));
        let name = sqlite_object_name(Path::new(&long));
        assert!(name.len() <= 180 + ".sqlite.zst.age".len() + 40);
        assert!(name.ends_with(".sqlite.zst.age"));
        assert!(name.contains('-'));
    }

    #[test]
    fn file_object_name_is_fixed() {
        assert_eq!(FILE_OBJECT, "files.tar.zst.age");
    }
}
