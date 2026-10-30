# oss-backup

把本地文件与 MySQL / PostgreSQL / SQLite 备份到阿里云 OSS 的单机 CLI 工具。

功能集参考 `D:\Work\webdav-backup`，但上传链路改为**全流式**：归档不落临时文件、不读进内存，内存占用只与分片大小相关。

## 特性

- 四类备份：文件目录、MySQL、PostgreSQL、SQLite（热备），一个 `[[backup]]` 可组合多种类型
- `[[backup]]` 没有独立名称：运行目录按 destination 分段，一个 destination 对应一个备份项
- 文件类：`walkdir` → glob 排除（每个 `[[backup.file]]` 自带 exclude）→ `tar` → `zstd` → `age` → OSS，边压边传
- 数据库类：`mysqldump` / `pg_dump` / `sqlite3 .backup` 输出直接管道压缩加密，不套 tar
- 层叠配置：`global` → `destination` → `backup`
- 全量快照，按 destination 隔离，应用端保留最近 N 份
- 上传完成后写 `manifest.json`（含 SHA-256、字节数、条目数），作为“本次运行完整”的提交标记
- 失败即 `abort()` 放弃分片上传，OSS 上不会留下可见半成品对象
- 进程级锁防重叠；可选 webhook 告警；明确退出码

## 要求

- Rust 1.85+
- `mysqldump` / `pg_dump` / `sqlite3`（按使用的备份类型安装，可用 `global.*_path` 指定完整路径）

## 构建

```bash
cargo build --release
```

Windows 静态编译（不依赖 MSVC 运行库）：

```powershell
$env:RUSTFLAGS='-C target-feature=+crt-static'
cargo build --release
```

## 使用

```bash
cp config.example.toml config.toml
# 编辑 config.toml，填入 endpoint / bucket / AK/SK / age 口令

# 收紧权限（含 AK/SK 与口令）
chmod 600 config.toml                                     # Linux
# icacls "config.toml" /inheritance:r /grant:r "%USERNAME%":F   # Windows

oss-backup --config config.toml --dry-run    # 只校验配置与连通性，打印计划写入的 key
oss-backup --config config.toml              # 执行备份
oss-backup --config config.toml --background # 日志写入 oss-backup.log
```

### 退出码

| 码 | 含义 |
|---|---|
| 0 | 全部成功 |
| 1 | 部分备份项失败 |
| 2 | 配置 / 凭据 / 文件权限错误 |
| 3 | 已有实例在运行（锁被占用） |

## 对象布局

```
{prefix}/{destination}/{YYYYMMDD_HHMMSS}/
├── files.tar.zst.age             # 文件类
├── {database}.mysql.zst.age      # MySQL，每个库一个对象
├── {database}.pgsql.zst.age      # PostgreSQL
├── {encoded_path}.sqlite.zst.age # SQLite，完整路径编码避免同名冲突
└── manifest.json                 # 最后写入
```

- 运行目录以 destination 名为段：同一个 destination 只能配置一个 `[[backup]]`
- SQLite 用完整路径编码：`D:\data\app.db` → `D__data_app.sqlite.zst.age`；路径过长时改用路径哈希
- 没有 `manifest.json` 的运行目录视为未完成，不参与保留计数；超过 24 小时会被回收

## 保留策略

`retain_count` 以“运行目录”为单位：保留最新 N 个完整运行，其余逐个删除；`0` 表示永不删除。
不使用 OSS 生命周期规则（它只能表达“保留 N 天”，表达不了“保留最近 N 份”）。

## 加密

产物使用 [age](https://age-encryption.org/) 口令模式加密（scrypt 派生），可用 `age` / `rage` 解密。

- 口令只从配置文件读取：`age_passphrase_file` 优先于明文 `age_passphrase`
- 不读取任何环境变量，避免多账号下“不知道用了哪个凭据”

## 手动恢复

工具本身不提供恢复命令，产物是标准格式，可离线解开：

```bash
# 文件类
age --decrypt -o files.tar.zst files.tar.zst.age    # 按提示输入口令
zstd -d files.tar.zst                               # 得到 files.tar
tar -xf files.tar                                   # 条目名形如 D__Documents/a.txt

# 数据库类
age --decrypt -o blog.sql.zst blog.mysql.zst.age
zstd -d blog.sql.zst                                # 得到 blog.sql，直接导入对应数据库
```

前提是 `config.toml` 里配置的 age 口令（或其口令文件）仍在手。

## 安全说明

- `config.toml` 是唯一密钥载体（AK/SK + age 口令 + 数据库口令），必须 600 / 仅本人可读
- 建议为备份单独创建 RAM 子账号，权限收敛到目标 bucket 的 `PutObject` / `AbortMultipartUpload` / `ListObjects` / `DeleteObject`
- 日志与报错中的 AK/SK 会打码
- Linux 下权限不合规直接拒绝启动；Windows 下检查 ACL 并给出 `icacls` 收紧命令

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 限制

- 全量快照，不做增量与去重
- 只支持阿里云 OSS，不暴露 `service` 配置
- 串行执行各 backup 项，单对象内分片也是串行上传
- 数据库导出依赖外部命令，且数据库口令以明文写在配置文件中
- 单次备份预期 < 50GB（默认 `part_size_mb = 16`，OSS 上限 10000 片）
