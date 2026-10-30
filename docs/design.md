# oss-backup 设计文档

> 全新重写的备份工具，功能集参考 `D:\Work\webdav-backup`（Rust CLI，WebDAV 目的地），
> 备份目的地改为阿里云 OSS，上传链路改为流式，内存 O(常量)。
>
> 状态：设计已与用户逐条确认（含第二轮修订），待审阅后开工。

## 1. 目标与非目标

**目标**

- 单机 CLI 备份工具，把本地文件与 MySQL / PostgreSQL / SQLite 数据库备份到阿里云 OSS
- 全量快照，每轮产出一个时间点备份，保留最近 N 份
- 全程流式：不把归档读进内存，也不落临时归档文件
- 失败可见：明确退出码 + 可选 webhook 告警

**非目标（第一版明确不做）**

- 不做恢复/下载/解包（`list` / `restore` / `verify` 子命令都没有）
- 不做多目的地同时写入（但支持配置多个 destination 供不同 backup 项选择）
- 不做增量备份与内容去重
- 不支持 OSS 以外的后端（不暴露 `service` 配置字段）
- 不做内置调度器（交给系统任务计划 / cron）
- 不做 STS / RAM Role 自动凭证
- 不从环境变量读取任何密钥（OSS AK/SK 与 age 口令统一走配置文件，见 §4.1）

## 2. 关键决策

| # | 决策点 | 结论 |
|---|---|---|
| 1 | 代码起点 | 全新重写，不复制参考工程 |
| 2 | 语言 | Rust（无 GC、常驻内存更低，且与现有项目工具链一致） |
| 3 | OSS 接入 | `opendal`（原生 OSS service + 流式写入 + Multipart + RetryLayer） |
| 4 | 备份类型 | 保留 file / MySQL / PgSQL / SQLite 四类 |
| 5 | 文件类产物 | `tar` + `zstd` 顺序流式写入 OSS，内存 O(1)，不落盘；**保留 tar 层**（普通文件大小已知，tar 头可在读内容前写出，是真流式；理由见 §5.1） |
| 6 | 数据库产物 | dump 输出直接管道给 zstd，产物 `{db}.mysql.zst.age` / `{db}.pgsql.zst.age` / `{name}.sqlite.zst.age`，**不套 tar 层**（tar 要求写入前已知文件大小，而 dump 是未知长度流） |
| 7 | 加密 | `age`（流式 AEAD，口令派生），产物后缀 `.age` |
| 8 | 配置/凭据 | TOML；**所有密钥（OSS AK/SK 与 age 口令）都写在配置文件内**（每个 destination 独立 AK/SK），启动强制校验文件权限、日志打码；不读环境变量 |
| 9 | 配置层级 | 三层继承：`global` → `destination` → `backup` |
| 10 | 备份模式 | 全量快照（每轮完整归档） |
| 11 | 对象布局 | `{destination.prefix}/{backup.name}/{YYYYMMDD_HHMMSS}/`，按 backup 名隔离 |
| 12 | 保留策略 | 应用端 `retain_count`，按“运行目录”为删除单位，绝不跨 prefix 误删 |
| 13 | 完整性 | 流式 SHA-256（对上传密文字节），旁挂 `manifest.json` |
| 14 | 并发 | backup 项串行；单对象上传交给 opendal + RetryLayer；进程级文件锁防重叠 |
| 15 | 调度/告警 | CLI 退出码 + 可选 webhook |
| 16 | CLI | 单命令 + `--config` / `--dry-run` / `--background` |
| 17 | 平台 | Windows + Linux，GitHub Actions 交叉编译静态二进制 |
| 18 | 验收 | 单测 + 真实 OSS bucket 手动验收；CI 只跑单测 |
| 19 | destination 数量 | 支持多个（不同 bucket / 不同子账号各写各的），每个自带 AK/SK |

## 3. 模块边界

```
src/
├── main.rs          入口：解析 CLI、加锁、初始化日志、遍历 backup 项、汇总退出码
├── cli.rs           clap 定义（--config / --dry-run / --background）
├── config/
│   ├── mod.rs       加载 TOML、三层继承解析、validate()、配置文件权限校验
│   └── model.rs     Global / Destination / Backup + 四类 payload 结构体
├── storage/
│   ├── mod.rs       OSS 操作封装（create_writer / list / delete / head）
│   └── oss.rs       opendal Operator 构建、endpoint 与凭证装配、重试层
├── archive/
│   ├── mod.rs       把「数据源」抽象成 Read 流，供 zstd/age 管道复用
│   ├── tar_zstd.rs  file：walkdir + exclude → tar → zstd
│   └── age.rs       age 加密 Writer 包装
├── db/
│   ├── mysql.rs     mysqldump 子进程 stdout → 流
│   ├── pgsql.rs     pg_dump 子进程 stdout → 流
│   └── sqlite.rs    sqlite3 .backup 到临时副本 → 流
├── retention.rs     列举运行目录、保留最新 N、删旧、清理孤儿
├── manifest.rs      manifest 生成与上传
├── naming.rs        对象名生成（路径编码、长度保护、非法字符替换）
├── lock.rs          进程级防重叠文件锁
├── notify.rs        webhook 推送（成功/失败）
└── logger.rs        tracing 初始化（控制台 / 文件）
```

**依赖方向**：`main → config / storage / archive / db / naming / retention / manifest`；
`archive` 与 `db` 只产出 `impl Read`，不认识 storage；`storage` 不认识备份类型。
这样“数据源 → 压缩 → 加密 → 上传”是一条扁平的 Reader/Writer 管道，没有回环。

## 4. 配置 schema

文件名默认 `config.toml`（可用 `--config` 指定）。

```toml
[global]
log_level = "info"            # trace|debug|info|warn|error
retain_count = 5              # 0 = 永不删除
zstd_level = 3                # 1-19，默认 3（取值参考见 §7.1）
part_size_mb = 16             # 分片大小，见 §9
timeout_secs = 1800           # 单对象上传超时

# age 口令：只从配置文件读取，优先级 file > 明文（与 AK/SK 同文件、同权限约束）
# age_passphrase_file = "D:\\secrets\\age.txt"
# age_passphrase = "..."                     # 明文（不推荐，启动时 warn）

webhook_url = ""              # 留空则不推送

# 可选：外部命令完整路径，留空则用 PATH 里的同名命令
# mysqldump_path = "C:\\mysql\\bin\\mysqldump.exe"
# pg_dump_path = "C:\\postgres\\bin\\pg_dump.exe"
# sqlite3_path = "C:\\sqlite\\sqlite3.exe"

[[destination]]
name = "aliyun"               # backup 项通过 name 引用
endpoint = "oss-cn-hangzhou.aliyuncs.com"
# internal = true             # 置 true 时自动改用 -internal 域名
bucket = "my-backup"
prefix = "backups"            # 全局前缀，可省略
access_key_id = "LTAI..."     # 必填，建议使用专用子账号
access_key_secret = "..."     # 必填
retain_count = 5              # 覆盖 global
zstd_level = 3                # 覆盖 global

[[destination]]               # 第二个 bucket / 第二个子账号
name = "archive"
endpoint = "oss-cn-hangzhou.aliyuncs.com"
bucket = "archive-bucket"
prefix = "backups"
access_key_id = "LTAI..."
access_key_secret = "..."

[[backup]]
name = "docs"                 # 必填且全局唯一，作为 OSS 路径段与保留单位
destination = "aliyun"
retain_count = 3              # 覆盖 destination

[backup.file]
local_path = ["D:\\Documents", "D:\\Photos"]
exclude = ["node_modules", "target", "*.tmp", "Thumbs.db"]

[[backup]]
name = "blog-db"
destination = "aliyun"

[backup.mysql]
host = "127.0.0.1"
port = 3306
username = "root"
password = "..."              # 数据库口令，沿用参考项目写法
database = ["blog.*", "shop"]
ssl_mode = "PREFERRED"        # 可选

[[backup]]
name = "cache"
destination = "aliyun"

[backup.sqlite]
database = ["D:\\data\\app.db", "D:\\data\\cache.db"]
```

### 4.1 凭据与权限

- 每个 destination 必须自带 `access_key_id` / `access_key_secret`，放在配置文件里；
  **不读取任何环境变量**，避免多账号时“配了没生效 / 不知道用了哪个”的歧义
- age 口令同样只从配置文件读取（`age_passphrase_file` 优先，其次 `age_passphrase`），
  与 AK/SK 共用同一份文件、同一套权限约束
- 启动即校验配置文件权限：
  - Linux：`mode & 0o077 != 0` → 直接失败，退出码 2，提示 `chmod 600 config.toml`
  - Windows：用 `icacls` 读取 ACL，若 `Everyone` / `Users` 可读 → **warn** 并打印收紧命令（不自动改 ACL）
- 仓库只提交 `config.example.toml`（占位值）；`.gitignore` 必须包含 `config.toml`
- 日志、manifest、webhook 中 AK/SK 一律打码（只显示前 4 + 后 4）
- 文档建议为备份单独建 RAM 子账号，权限收敛到目标 bucket 的 `PutObject` / `AbortMultipartUpload` /
  `ListObjects` / `DeleteObject`，不要用主账号 AK

**继承规则**

- `retain_count` / `zstd_level` / `timeout_secs` / `part_size_mb` / `age_passphrase*`：
  依次取 backup、destination、global，都缺则用内置默认值
- `prefix`：destination 级唯一，backup 级不再支持独立 prefix（路径隔离靠 `backup.name`）
- 缺失必填字段直接报错并指出哪个 `backup[i]` / `destination[j]`

**校验（`config::validate`）**

- `backup.name` 全局唯一，非空，只允许 `[A-Za-z0-9._-]`
- `backup.destination` 必须能匹配某个 destination
- 每个 backup 项必须且只能有一个 payload（file / mysql / pgsql / sqlite）
- `file.local_path` 非空；`mysql/pgsql.database` 非空；`sqlite.database` 非空
- 每个 destination 的 `endpoint` / `bucket` / `access_key_id` / `access_key_secret` 非空
- `part_size_mb` 与实际数据规模之间的一致性检查见 §9

## 5. 对象布局与 manifest

**统一以“运行目录”为单位**，所有类型都落到一个时间戳目录：

```
{prefix}/{backup.name}/{YYYYMMDD_HHMMSS}/
├── files.tar.zst.age             # file 类型（固定名，单对象）
├── blog.mysql.zst.age            # mysql：每个库一个对象
├── shop.mysql.zst.age
├── sales.pgsql.zst.age           # pgsql：每个库一个对象
├── D__data_app.sqlite.zst.age    # sqlite：由完整路径编码，避免同名冲突
└── manifest.json                 # 最后上传，作为“本次运行完整”的提交标记
```

**命名规则（`naming.rs`）**

| 类型 | 对象名 | 说明 |
|---|---|---|
| file | `files.tar.zst.age` | 固定，一轮一个对象 |
| mysql | `{database}.mysql.zst.age` | 库名白名单 `[A-Za-z0-9._-]`，其余字符替换为 `_` |
| pgsql | `{database}.pgsql.zst.age` | 同上 |
| sqlite | `{encoded_path}.sqlite.zst.age` | 用**完整路径**编码：`/ \ :` 全替换为 `_`，如 `D:\data\app.db` → `D__data_app.sqlite.zst.age` |

- sqlite 必须用完整路径：不同目录下同名 `app.db` 否则会互相覆盖
- 编码后超过 180 字节时，改为 `{stem}-{sha256(path)[0..8]}.sqlite.zst.age`，原始路径写入 manifest
- 任何对象名超长/非法，直接报错而不是静默截断

**说明与取舍**

- 文件类也放进时间戳目录，与数据库类保持同一套保留/清理语义；代价是多一层路径
- 多库场景下一轮运行产生多个对象，保留仍以“运行”为单位，语义与参考项目的“一份 zip”对齐
- `manifest.json` 最后上传：没 manifest 的目录视为未完成的运行，不参与保留计数

### 5.1 为什么 file 类保留 tar，而不是“随读随传”

- tar 是唯一能“多文件装进单对象、且顺序写即可”的通用容器；普通文件大小已知，
  tar 头可以在读内容前就写好，因此不需要 seek，是真流式
- 丢掉 tar 只剩两条路：
  1. 每个文件一个 OSS 对象 → 对象数可达百万级，列举、保留、请求计费、恢复管理全部变差
  2. 自研顺序容器 → 实现与兼容性成本远超收益
- tar 本身的额外开销很小（每条目约 512B 头 + 块对齐 padding），真正的 CPU 消耗在 zstd，
  保留 tar 不会成为瓶颈

**manifest schema**

```json
{
  "version": 1,
  "backup": "docs",
  "kind": "file",
  "destination": "aliyun",
  "started_at": "2026-10-30T02:00:00+08:00",
  "finished_at": "2026-10-30T02:03:12+08:00",
  "zstd_level": 3,
  "encryption": "age-passphrase",
  "inputs": ["D:\\Documents", "D:\\Photos"],
  "objects": [
    {
      "key": "backups/docs/20261030_020000/files.tar.zst.age",
      "source": null,             // 数据库类记原始库名/文件路径，file 类为 null
      "size": 123456789,
      "sha256": "…",              // 对上传的密文字节
      "plaintext_bytes": 234567890,
      "entries": 12034            // file 类型为文件数，db 类型为 1
    }
  ],
  "total_plaintext_bytes": 234567890,
  "duration_ms": 192000,
  "tool_version": "0.1.0"
}
```

**保留（`retention.rs`）**

1. `ListObjectsV2` 递归列举 `{prefix}/{backup.name}/` 下所有对象
2. 按路径第二段（时间戳目录）分组，只保留带 `manifest.json` 的完整运行
3. 按目录名时间戳倒序，保留最新 `retain_count` 个，其余逐个 `DELETE`
4. 超过 24 小时仍无 manifest 的孤儿目录，记 warn 并删除
5. `retain_count = 0` 直接跳过

不依赖 OSS 生命周期规则（只能表达“保留 N 天”，表达不了“保留最近 N 份”）。

## 6. 流水线

**file 类型**

```
walkdir(local_path, follow_links=false)
  → 过滤 exclude(globset)
  → tar::Builder（保留相对路径前缀、mode、mtime）
  → zstd Encoder(level)
  → age Encryptor(passphrase)
  → 边算 SHA-256 边喂给 opendal Writer
  → CompleteMultipartUpload
```

**数据库类型**

```
mysqldump / pg_dump / sqlite3 .backup  →  stdout
  → zstd Encoder → age Encryptor → SHA-256 → opendal Writer
```

多库时逐个库顺序执行，每个库一个对象；全部成功后才上传 manifest。

**失败处理**

- 任一环节 Err → 调用 `writer.abort()` 放弃 Multipart，OSS 上不产生可见对象
- 已成功的对象保留（孤儿目录由 retention 的 24 小时规则回收），manifest 不上传
- 数据库子进程非 0 退出 → 带上 stderr 尾部 20 行作为错误信息

**内存模型**

- tar/zstd/age/opendal 各层都只持有小块缓冲，总内存 ≈ 数个 part 大小（几十 MB 量级）
- 不使用 `tokio::fs::read` / `read_to_end` 读归档；该写法是参考项目的核心缺陷

## 7. 压缩与加密

### 7.1 zstd_level 取值参考

| 级别 | 单核吞吐（经验值） | 典型压缩率 | 压缩内存 | 适用场景 |
|---|---|---|---|---|
| `-5..-1` | > 500MB/s | 1.5–2x | < 10MB | 仅应急，几乎没有压缩收益 |
| `1–3` | 300–500MB/s | 2.5–3.5x | 10–30MB | **默认 3**；照片/视频/zip 等已压缩数据 |
| `4–9` | 100–250MB/s | 3–4.5x | 30–80MB | 文本/源码，**SQL dump 建议 6–9** |
| `10–15` | 30–80MB/s | 4–5x | 100–250MB | 冷数据归档，能接受长时间占用 CPU |
| `16–19` | 10–20MB/s | 5–6x | 400–700MB（19 级窗口 128MB） | **不推荐**：直接破坏“低内存”前提 |

- 选级原则：备份是 IO 密集，压缩比只换到一点存储费；默认 3，文本类可上调到 6–9
- 数值为通用经验值，实际随数据类型和 CPU 变化；文档不承诺精确吞吐

### 7.2 age

- 算法与格式：`age` 口令模式（scrypt 派生），产物兼容 `age` / `rage` 命令行
- 口令只从配置文件读取：`age_passphrase_file` 优先，其次 `age_passphrase`（明文触发 warn）；
  不读环境变量，与 AK/SK 同一份文件、同一权限约束
- 不做收件人（X25519）模式：第一版用口令，跨机器解密更省事
- OSS 侧不启用 SSE-KMS（客户端已加密，避免双份密钥管理）；文档里说明可按需开启

## 8. 可靠性

- **重试**：opendal `RetryLayer`（指数退避，默认 5 次）+ 显式超时（`timeout_secs`）
- **防重叠**：启动即对 `{config 所在目录}/.oss-backup.lock` 取排他锁；拿不到锁 → 退出码 3，不执行任何上传
- **退出码**：`0` 全部成功 / `1` 部分失败 / `2` 配置或凭据错误 / `3` 已在运行
- **日志**：tracing；`--background` 时写 `oss-backup.log`，前台输出到控制台
- **webhook**：POST JSON，`{status, backup_count, failures:[{name,error}], duration_ms}`；成功与失败都推，推送失败仅 warn，不影响退出码

## 9. 分片与数据量

- 默认 `part_size_mb = 16`（OSS 单对象最多 10000 片，理论 160GB）
- 单次备份预期 < 50GB，默认参数足够；`--dry-run` 会预估待备份字节数，若 `预估值 / part_size > 10000` 直接报错并建议调大 `part_size_mb`
- opendal Writer 的 chunk 写入与 OSS Multipart 的自动切换行为在实现阶段用真实桶验证，若不满足则退化为显式 Multipart 封装

## 10. CLI

```
oss-backup [--config config.toml] [--dry-run] [--background]
```

- `--dry-run`：加载 + 校验配置与文件权限、解析 AK/SK、列举 bucket 前缀验证权限、打印将写入的对象 key 与预估大小；不创建任何对象
- `--background`：日志写文件
- 前台运行时日志带时间戳与级别，`info` 级只输出每项的起止与结果

## 11. 测试与验收

**单测（CI 跑）**

- 配置：三层继承各字段、缺字段报错、payload 数量校验、name 唯一性、AK/SK 除空校验
- 权限：Linux 权限位判定（`0o644` 拒绝 / `0o600` 通过），Windows 分支只测 warn 逻辑
- key 生成：时间戳目录、prefix 拼接、非法 name 拒绝、sqlite 完整路径编码、同名不同目录不冲突、超长名走哈希兜底
- retention：构造对象列表 → 验证保留/删除集合、孤儿目录处理、`retain_count=0`
- archive：临时目录建 tree → tar.zst 产出 → 用 tar/zstd 读回校验条目、排除规则、空目录与符号链接处理
- age：加密后可解密回原文（小样本）
- manifest：字段完整性、sha256 与 size 一致性、AK 打码

**手动验收（需用户提供测试 bucket，不在 CI 跑）**

1. `--dry-run` 通过且输出预期 key
2. 一个 file 项真实备份成功，manifest 的 `size` / `sha256` 与桶内对象一致
3. 一个 MySQL 项真实备份成功，多库产出多对象，文件名带 `mysql`
4. 两个不同目录下的同名 SQLite 文件，产出两个不同对象名且内容可区分
5. 连续跑 N+1 次，旧运行目录被删除，只剩 N 个
6. 中途断网 → 退出码 1，桶内无可见半成品对象，无 manifest
7. 同时启动两个进程 → 第二个退出码 3
8. 把 `config.toml` 权限改成 644 → 退出码 2

**CI**

- `cargo fmt --check` / `cargo clippy -D warnings` / `cargo test`
- tag 触发：`windows-x86_64`（`+crt-static`）、`linux-x86_64`（musl）静态二进制

## 12. 风险与待验证项

| 风险 | 影响 | 处理 |
|---|---|---|
| opendal OSS Writer 的 Multipart 触发条件与 `abort()` 语义 | 断网可能留下不可见分片或半成品 | 实现前先用最小样例验证；必要时显式封装 Multipart |
| `age` crate 流式写入 API 与 `zstd` 组合的背压 | 可能内存上涨或 CPU 打满 | 优先用阻塞管道 + `spawn_blocking`，限制缓冲 |
| Windows 静态编译（opendal 依赖链） | CI 构建失败 | 先跑通本地 `+crt-static` 构建，再上 CI |
| **config.toml 含明文 AK/SK** | 文件被复制/误提交即泄露 | 启动权限校验 + 日志打码 + `.gitignore`；文档要求专用子账号最小权限。权限校验只能降低不能消除风险 |
| 数据库口令仍为明文 in config | 泄露风险（与 AK/SK 同一份文件） | 同上一行；后续版本可加 `password_env` |
| Windows ACL 校验只能 warn | 用户可能忽略提示 | 文档给出 `icacls` 收紧命令；启动日志明确标注“配置文件可被其他用户读取” |

## 13. 里程碑

1. 工程骨架 + config/model/validate/权限校验 + 单测
2. storage/oss（Operator 构建、AK/SK 装配、`--dry-run` 连通性）
3. file 全链路（walkdir → tar → zstd → age → 上传 → manifest）
4. retention + lock + 退出码 + webhook
5. 三类数据库 + naming（sqlite 路径编码）
6. 单测补齐 + README + `config.example.toml` + CI 矩阵
