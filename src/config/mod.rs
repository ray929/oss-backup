use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub mod model;

pub use model::*;

pub const DEFAULT_RETAIN_COUNT: usize = 0;
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const DEFAULT_PART_SIZE_MB: u64 = 16;
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// 单个 backup 项解析后的有效设置（已完成三层继承）
#[derive(Debug, Clone)]
pub struct BackupSettings {
    pub retain_count: usize,
    pub zstd_level: i32,
    pub part_size_mb: u64,
    pub timeout_secs: u64,
    pub passphrase: String,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let mut value = load_value(path)
            .with_context(|| "无法加载主配置文件，可从 config.example.jsonc 复制一份")?;
        let base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut stack = vec![canonical_key(path)];
        expand_imports(&mut value, base_dir, &mut stack, 0)?;
        serde_json::from_value(value)
            .with_context(|| format!("配置文件字段不合法：{}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.destination.is_empty() {
            bail!("配置缺少 destination");
        }
        if self.backup.is_empty() {
            bail!("配置缺少 backup");
        }

        let mut dest_names = HashSet::new();
        for (idx, dest) in self.destination.iter().enumerate() {
            if !crate::naming::is_valid_name(&dest.name) {
                bail!(
                    "destination[{idx}] 名称 '{}' 非法（只允许字母、数字、点、下划线、短横线）",
                    dest.name
                );
            }
            if !dest_names.insert(dest.name.as_str()) {
                bail!("destination 名称重复：{}", dest.name);
            }
            for (field, value) in [
                ("endpoint", &dest.endpoint),
                ("bucket", &dest.bucket),
                ("access_key_id", &dest.access_key_id),
                ("access_key_secret", &dest.access_key_secret),
            ] {
                if value.trim().is_empty() {
                    bail!("destination[{}].{} 不能为空", idx, field);
                }
            }
            if let Some(prefix) = &dest.prefix {
                if prefix.starts_with('/') {
                    bail!("destination[{}].prefix 不能以 '/' 开头", idx);
                }
            }
        }

        // 先解析每项实际使用的 destination（省略即第一个），并统计被引用次数
        let mut effective: Vec<&str> = Vec::with_capacity(self.backup.len());
        let mut usage: HashMap<&str, usize> = HashMap::new();
        for (idx, backup) in self.backup.iter().enumerate() {
            let dest = self
                .destination(backup)
                .with_context(|| format!("backup[{idx}] 的 destination 无法解析"))?;
            effective.push(dest.name.as_str());
            *usage.entry(dest.name.as_str()).or_insert(0) += 1;
        }

        let mut names: HashSet<(&str, &str)> = HashSet::new();
        for (idx, backup) in self.backup.iter().enumerate() {
            let dest_name = effective[idx];
            let shared = usage.get(dest_name).copied().unwrap_or(0) > 1;

            // 一个 destination 被多项共用时，运行目录必须靠 name 区分，否则
            // retention 会按同一命名空间互删，同秒启动还会覆盖 manifest。
            match &backup.name {
                Some(name) => {
                    if !crate::naming::is_valid_name(name) {
                        bail!(
                            "backup[{idx}].name '{name}' 非法（只允许字母、数字、点、下划线、短横线）"
                        );
                    }
                    if !names.insert((dest_name, name.as_str())) {
                        bail!("destination '{dest_name}' 下的 backup 名称重复：{name}");
                    }
                }
                None if shared => bail!(
                    "destination '{dest_name}' 被 {} 个 backup 共用，backup[{idx}] 必须设置 name",
                    usage[dest_name]
                ),
                None => {}
            }

            let kinds = [
                !backup.files.is_empty(),
                backup.mysql.is_some(),
                backup.pgsql.is_some(),
                !backup.sqlite.is_empty(),
            ];
            let count = kinds.iter().filter(|v| **v).count();
            if count == 0 {
                bail!("backup[{idx}] 必须包含 files / mysql / pgsql / sqlite 至少一种");
            }

            for (spec_idx, file) in backup.files.iter().enumerate() {
                if file.local_path.as_os_str().is_empty() {
                    bail!("backup[{idx}].files[{spec_idx}].local_path 不能为空");
                }
            }
            if let Some(mysql) = &backup.mysql {
                if mysql.database.is_empty() {
                    bail!("backup[{idx}].mysql.database 不能为空");
                }
                validate_db_entries("mysql", idx, &mysql.database)?;
            }
            if let Some(pgsql) = &backup.pgsql {
                if pgsql.database.is_empty() {
                    bail!("backup[{idx}].pgsql.database 不能为空");
                }
                validate_db_entries("pgsql", idx, &pgsql.database)?;
            }

            self.settings(backup)
                .with_context(|| format!("backup[{idx}] 设置解析失败"))?;
        }

        Ok(())
    }

    /// 解析 backup 实际使用的 destination：显式指定，否则取配置里的第一个
    pub fn destination(&self, backup: &Backup) -> Result<&Destination> {
        match backup.destination.as_deref() {
            Some(name) => self
                .destination
                .iter()
                .find(|d| d.name == name)
                .with_context(|| format!("找不到 destination '{name}'")),
            None => self.destination.first().context("配置缺少 destination"),
        }
    }

    pub fn settings(&self, backup: &Backup) -> Result<BackupSettings> {
        let dest = self.destination(backup)?;

        let zstd_level = backup
            .zstd_level
            .or(dest.zstd_level)
            .or(self.global.zstd_level)
            .unwrap_or(DEFAULT_ZSTD_LEVEL);
        // 上限卡在 9：zstd 16 级以上的压缩窗口能到数百 MB，会直接突破内存预算
        if !(-7..=9).contains(&zstd_level) {
            bail!(
                "zstd_level 必须在 -7..=9 之间，当前为 {zstd_level}（更高层级的压缩窗口会突破内存预算）"
            );
        }

        let part_size_mb = backup
            .part_size_mb
            .or(dest.part_size_mb)
            .or(self.global.part_size_mb)
            .unwrap_or(DEFAULT_PART_SIZE_MB);
        if part_size_mb == 0 {
            bail!("part_size_mb 必须大于 0");
        }

        let timeout_secs = backup
            .timeout_secs
            .or(dest.timeout_secs)
            .or(self.global.timeout_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        if timeout_secs == 0 {
            bail!("timeout_secs 必须大于 0");
        }

        let retain_count = backup
            .retain_count
            .or(dest.retain_count)
            .or(self.global.retain_count)
            .unwrap_or(DEFAULT_RETAIN_COUNT);

        let passphrase = self.resolve_passphrase(backup)?;

        Ok(BackupSettings {
            retain_count,
            zstd_level,
            part_size_mb,
            timeout_secs,
            passphrase,
        })
    }

    pub fn mysqldump_path(&self) -> &str {
        self.global.mysqldump_path.as_deref().unwrap_or("mysqldump")
    }

    pub fn pg_dump_path(&self) -> &str {
        self.global.pg_dump_path.as_deref().unwrap_or("pg_dump")
    }

    pub fn sqlite3_path(&self) -> &str {
        self.global.sqlite3_path.as_deref().unwrap_or("sqlite3")
    }

    pub fn resolve_passphrase(&self, backup: &Backup) -> Result<String> {
        if let Some(path) = backup
            .age_passphrase_file
            .as_ref()
            .or(self.global.age_passphrase_file.as_ref())
        {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("读取 age 口令文件失败：{}", path.display()))?;
            let passphrase = text.trim_end_matches(['\r', '\n']).to_string();
            if passphrase.is_empty() {
                bail!("age 口令文件为空：{}", path.display());
            }
            return Ok(passphrase);
        }

        match backup
            .age_passphrase
            .as_ref()
            .or(self.global.age_passphrase.as_ref())
        {
            Some(p) if !p.is_empty() => {
                tracing::warn!("使用配置文件内的明文 age 口令，建议改用 age_passphrase_file");
                Ok(p.clone())
            }
            _ => bail!("未配置 age 口令（age_passphrase_file 或 age_passphrase）"),
        }
    }
}

const MAX_IMPORT_DEPTH: usize = 8;

fn canonical_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn load_value(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        bail!("配置文件不存在：{}", path.display());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    jsonc_parser::parse_to_serde_value(&text, &Default::default())
        .with_context(|| format!("解析配置文件失败：{}", path.display()))
}

/// 把 backup 数组里的 `{"import": "path"}` 展开成被引用文件中的 backup 项。
///
/// 被引用文件可以是 backup 项数组，也可以是只含 `backup` 键的对象；其中若还有
/// import 会继续递归展开。相对路径以**引用方文件所在目录**为基准；
/// 循环引用和超过 8 层嵌套直接报错。
fn expand_imports(
    value: &mut serde_json::Value,
    base_dir: &Path,
    stack: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_IMPORT_DEPTH {
        bail!("import 嵌套超过 {MAX_IMPORT_DEPTH} 层，检查是否写成了链式引用");
    }
    if let Some(backups) = value.get_mut("backup").and_then(|v| v.as_array_mut()) {
        expand_items(backups, base_dir, stack, depth)?;
    }
    Ok(())
}

fn expand_items(
    items: &mut Vec<serde_json::Value>,
    base_dir: &Path,
    stack: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_IMPORT_DEPTH {
        bail!("import 嵌套超过 {MAX_IMPORT_DEPTH} 层，检查是否写成了链式引用");
    }

    let mut expanded = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        let Some(rel) = parse_import(&item)? else {
            expanded.push(item);
            continue;
        };

        let path = if Path::new(&rel).is_absolute() {
            PathBuf::from(&rel)
        } else {
            base_dir.join(&rel)
        };
        let key = canonical_key(&path);
        if stack.contains(&key) {
            bail!("import 循环引用：{}", path.display());
        }

        let sub = load_value(&path)?;
        let sub_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut sub_items = import_items(sub, &path)?;

        stack.push(key);
        expand_items(&mut sub_items, sub_dir, stack, depth + 1)?;
        stack.pop();

        expanded.extend(sub_items);
    }

    *items = expanded;
    Ok(())
}

/// 只识别严格的 `{"import": "..."}`；带其他键就报错，避免语义含糊
fn parse_import(item: &serde_json::Value) -> Result<Option<String>> {
    let Some(obj) = item.as_object() else {
        return Ok(None);
    };
    let Some(import) = obj.get("import") else {
        return Ok(None);
    };
    if obj.len() != 1 {
        let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        bail!("import 项不能带其他字段，当前为 {keys:?}");
    }
    match import.as_str() {
        Some(path) if !path.trim().is_empty() => Ok(Some(path.to_string())),
        _ => bail!("import 必须是非空字符串路径，当前为 {import}"),
    }
}

fn import_items(value: serde_json::Value, path: &Path) -> Result<Vec<serde_json::Value>> {
    match value {
        serde_json::Value::Array(items) => Ok(items),
        serde_json::Value::Object(mut obj) => {
            let extra: Vec<String> = obj
                .keys()
                .filter(|k| k.as_str() != "backup")
                .cloned()
                .collect();
            if !extra.is_empty() {
                bail!(
                    "被导入文件 {} 只能包含 backup 项数组，或只含 backup 键的对象；多出：{}",
                    path.display(),
                    extra.join(", ")
                );
            }
            match obj.remove("backup") {
                Some(serde_json::Value::Array(items)) => Ok(items),
                Some(other) => bail!(
                    "被导入文件 {} 的 backup 必须是数组，当前为 {other}",
                    path.display()
                ),
                None => bail!("被导入文件 {} 缺少 backup 键", path.display()),
            }
        }
        other => bail!(
            "被导入文件 {} 顶层必须是数组或对象，当前为 {other}",
            path.display()
        ),
    }
}

/// 校验 `db` / `db.*` / `db.table` 条目，拒绝空段（如 `blog.` 会被解析成空表名）
fn validate_db_entries(kind: &str, idx: usize, entries: &[String]) -> Result<()> {
    for entry in entries {
        if entry.split('.').any(|part| part.trim().is_empty()) {
            bail!("backup[{idx}].{kind}.database 条目 '{entry}' 含空段；整库请写 'db' 或 'db.*'");
        }
    }
    Ok(())
}

/// 配置文件权限校验。
///
/// Linux 下强制 600；Windows 下用 `icacls` 探测过宽的 ACL 并给出告警。
pub fn check_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("读取配置文件属性失败：{}", path.display()))?
            .permissions()
            .mode();
        if !is_private_mode(mode) {
            bail!(
                "配置文件权限过宽（{:o}），其中包含 AK/SK 与口令，请执行：chmod 600 {}",
                mode & 0o777,
                path.display()
            );
        }
    }

    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("icacls").arg(path).output() {
            let text = String::from_utf8_lossy(&output.stdout);
            if text.contains("Everyone") || text.contains("BUILTIN\\Users") {
                tracing::warn!(
                    "配置文件可被其他用户读取，其中包含 AK/SK 与口令，建议执行：icacls \"{}\" /inheritance:r /grant:r \"%USERNAME%\":F",
                    path.display()
                );
            }
        }
    }

    Ok(())
}

/// 纯函数：Unix 权限是否只允许属主访问
#[cfg_attr(not(unix), allow(dead_code))]
pub fn is_private_mode(mode: u32) -> bool {
    mode & 0o077 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(text: &str) -> Config {
        let value = jsonc_parser::parse_to_serde_value(text, &Default::default()).unwrap();
        serde_json::from_value(value).expect("config should parse")
    }

    fn minimal() -> Config {
        parse(
            r#"{
  // JSONC 允许注释
  "global": { "age_passphrase": "pw" },
  "destination": [
    {
      "name": "aliyun",
      "endpoint": "oss-cn-hangzhou.aliyuncs.com",
      "bucket": "b",
      "access_key_id": "id",
      "access_key_secret": "sec"
    }
  ],
  "backup": [
    {
      "destination": "aliyun",
      "files": [
        { "local_path": "D:/docs", "exclude": ["node_modules", "*.tmp"] }
      ]
    }
  ]
}"#,
        )
    }

    #[test]
    fn parses_file_sources_with_own_excludes() {
        let cfg = minimal();
        assert_eq!(cfg.backup[0].files.len(), 1);
        assert_eq!(cfg.backup[0].files[0].local_path, PathBuf::from("D:/docs"));
        assert_eq!(cfg.backup[0].files[0].exclude.len(), 2);
    }

    #[test]
    fn parses_multiple_file_sources_and_trailing_comma() {
        let cfg = parse(
            r#"{
  "global": { "age_passphrase": "pw" },
  "destination": [
    { "name": "aliyun", "endpoint": "e", "bucket": "b", "access_key_id": "id", "access_key_secret": "sec" },
  ],
  "backup": [
    {
      "destination": "aliyun",
      "files": [
        { "local_path": "D:/a", "exclude": ["*.tmp"] },
        { "local_path": "D:/b" },
      ],
      "sqlite": ["D:/a.db"],
    },
  ],
}"#,
        );
        assert_eq!(cfg.backup[0].files.len(), 2);
        assert!(cfg.backup[0].files[1].exclude.is_empty());
        assert_eq!(cfg.backup[0].sqlite, vec![PathBuf::from("D:/a.db")]);
    }

    #[test]
    fn inheritance_prefers_backup_then_destination_then_global() {
        let mut cfg = minimal();
        cfg.global.zstd_level = Some(3);
        cfg.global.retain_count = Some(5);
        cfg.destination[0].zstd_level = Some(6);
        cfg.destination[0].retain_count = Some(4);
        cfg.backup[0].retain_count = Some(2);

        let settings = cfg.settings(&cfg.backup[0]).unwrap();
        assert_eq!(settings.zstd_level, 6);
        assert_eq!(settings.retain_count, 2);
        assert_eq!(settings.part_size_mb, DEFAULT_PART_SIZE_MB);
    }

    #[test]
    fn rejects_unknown_destination() {
        let mut cfg = minimal();
        cfg.backup[0].destination = Some("nope".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn allows_multiple_payloads() {
        let mut cfg = minimal();
        cfg.backup[0].sqlite = vec![PathBuf::from("a.db")];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_shared_destination_without_name() {
        let mut cfg = minimal();
        let mut second = cfg.backup[0].clone();
        second.files.clear();
        second.sqlite = vec![PathBuf::from("a.db")];
        cfg.backup.push(second);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("必须设置 name"), "{err}");
    }

    #[test]
    fn allows_shared_destination_when_named() {
        let mut cfg = minimal();
        cfg.backup[0].name = Some("docs".into());
        let mut second = cfg.backup[0].clone();
        second.name = Some("db".into());
        second.files.clear();
        second.sqlite = vec![PathBuf::from("a.db")];
        cfg.backup.push(second);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn missing_destination_falls_back_to_first() {
        let mut cfg = minimal();
        cfg.backup[0].destination = None;
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.destination(&cfg.backup[0]).unwrap().name, "aliyun");
    }

    #[test]
    fn imports_backup_items_from_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("extra.jsonc"),
            r#"[{ "name": "extra", "destination": "aliyun", "sqlite": ["D:/a.db"] }]"#,
        )
        .unwrap();

        let main = dir.path().join("config.jsonc");
        std::fs::write(
            &main,
            r#"{
  "global": { "age_passphrase": "pw" },
  "destination": [{ "name": "aliyun", "endpoint": "e", "bucket": "b", "access_key_id": "id", "access_key_secret": "sec" }],
  "backup": [
    { "import": "extra.jsonc" },
    { "name": "docs", "destination": "aliyun", "files": [{ "local_path": "D:/docs" }] }
  ]
}"#,
        )
        .unwrap();

        let cfg = Config::load(&main).unwrap();
        assert_eq!(cfg.backup.len(), 2);
        assert_eq!(cfg.backup[0].name.as_deref(), Some("extra"));
        assert_eq!(cfg.backup[1].name.as_deref(), Some("docs"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn example_config_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.jsonc");
        let cfg = Config::load(&path).expect("示例配置必须能被解析");
        assert_eq!(cfg.backup.len(), 1);
        assert_eq!(cfg.backup[0].name.as_deref(), Some("docs"));
    }

    #[test]
    fn rejects_import_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jsonc");
        let b = dir.path().join("b.jsonc");
        std::fs::write(
            &a,
            r#"{ "global": {"age_passphrase":"pw"}, "destination": [], "backup": [{ "import": "b.jsonc" }] }"#,
        )
        .unwrap();
        std::fs::write(&b, r#"[{ "import": "a.jsonc" }]"#).unwrap();

        let err = Config::load(&a).unwrap_err().to_string();
        assert!(err.contains("循环引用"), "{err}");
    }

    #[test]
    fn rejects_import_with_extra_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.jsonc");
        std::fs::write(
            &p,
            r#"{ "global": {"age_passphrase":"pw"}, "destination": [], "backup": [{ "import": "x.jsonc", "name": "n" }] }"#,
        )
        .unwrap();

        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("不能带其他字段"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = r#"{
  "global": { "age_passpharse": "typo" },
  "destination": [],
  "backup": []
}"#;
        let value = jsonc_parser::parse_to_serde_value(text, &Default::default()).unwrap();
        assert!(
            serde_json::from_value::<Config>(value).is_err(),
            "拼错的键名必须报错，而不是被静默忽略"
        );
    }

    #[test]
    fn rejects_empty_table_segment() {
        let mut cfg = minimal();
        cfg.backup[0].mysql = Some(crate::config::MysqlCfg {
            host: "127.0.0.1".into(),
            port: 3306,
            username: "u".into(),
            password: "p".into(),
            database: vec!["blog.".into()],
            ssl_mode: None,
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_no_payload() {
        let mut cfg = minimal();
        cfg.backup[0].files.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_out_of_range_zstd_level() {
        let mut cfg = minimal();
        cfg.global.zstd_level = Some(30);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn missing_passphrase_is_error() {
        let mut cfg = minimal();
        cfg.global.age_passphrase = None;
        assert!(cfg.resolve_passphrase(&cfg.backup[0]).is_err());
    }

    #[test]
    fn private_mode_check() {
        assert!(is_private_mode(0o600));
        assert!(!is_private_mode(0o644));
        assert!(!is_private_mode(0o640));
        assert!(is_private_mode(0o400));
    }

    #[test]
    fn passphrase_file_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pw.txt");
        std::fs::write(&p, "from-file\n").unwrap();
        let mut cfg = minimal();
        cfg.global.age_passphrase = Some("literal".into());
        cfg.global.age_passphrase_file = Some(p);
        assert_eq!(cfg.resolve_passphrase(&cfg.backup[0]).unwrap(), "from-file");
    }
}
