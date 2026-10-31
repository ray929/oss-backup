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

/// 运行目录：`{prefix}/{name}/{timestamp}`；name 省略时少一层
pub fn run_dir(prefix: Option<&str>, name: Option<&str>, timestamp: &str) -> String {
    join_key(&[prefix.unwrap_or(""), name.unwrap_or(""), timestamp])
}

/// 保留策略的列举前缀：`{prefix}/{name}/`；都没配时为整个 bucket
pub fn run_prefix(prefix: Option<&str>, name: Option<&str>) -> String {
    let base = join_key(&[prefix.unwrap_or(""), name.unwrap_or("")]);
    if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    }
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
///
/// 编码规则：先把 `_` 转义成 `__`，再把 `/` `\` `:` 替换成 `_`，原扩展名保留。
/// 不能简单地把分隔符都换成 `_`——那样 `D:\a\app.db` 与 `D:\a_app.db` 会撞成
/// 同一个对象名，后者会静默覆盖前者；去掉扩展名还会让 `app.db` 与 `app.sqlite3` 撞名。
pub fn sqlite_object_name(path: &Path) -> String {
    let raw = path.to_string_lossy();
    // 只去掉路径开头那几个分隔符。编码后再 trim 会把转义出来的 `__` 一起吃掉了。
    let stripped = raw.trim_start_matches(['/', '\\']);

    let mut encoded = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        match c {
            '_' => encoded.push_str("__"),
            '/' | '\\' | ':' => encoded.push('_'),
            other => encoded.push(other),
        }
    }

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
            run_dir(Some("backups"), None, "20260101_010101"),
            "backups/20260101_010101"
        );
        assert_eq!(
            run_dir(Some("backups"), Some("docs"), "20260101_010101"),
            "backups/docs/20260101_010101"
        );
        assert_eq!(run_dir(None, None, "20260101_010101"), "20260101_010101");
        assert_eq!(run_prefix(Some("backups"), None), "backups/");
        assert_eq!(run_prefix(Some("backups"), Some("docs")), "backups/docs/");
        assert_eq!(run_prefix(None, Some("docs")), "docs/");
        assert_eq!(run_prefix(None, None), "");
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
            "D__data_app.db.sqlite.zst.age"
        );
        assert_eq!(
            sqlite_object_name(Path::new("/var/lib/app.db")),
            "var_lib_app.db.sqlite.zst.age"
        );
    }

    #[test]
    fn sqlite_encoding_does_not_collide() {
        let a = sqlite_object_name(Path::new(r"D:\a\app.db"));
        let b = sqlite_object_name(Path::new(r"D:\a_app.db"));
        let c = sqlite_object_name(Path::new(r"D:\a\app.sqlite3"));

        assert_eq!(a, "D__a_app.db.sqlite.zst.age");
        assert_eq!(b, "D__a__app.db.sqlite.zst.age");
        assert_eq!(c, "D__a_app.sqlite3.sqlite.zst.age");
        assert_ne!(a, b, "下划线不能被当成路径分隔符");
        assert_ne!(a, c, "不同扩展名不能撞名");
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
