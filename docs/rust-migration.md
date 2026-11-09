# Rust 原生迁移

Rust 实现覆盖 Publisher、Webhook、对象读取、恢复清理、Info、宿主机采集和运维命令。浏览器继续使用现有 HTML、CSS 和 JavaScript；Git/SSH、Nginx、Gitea 和对象存储继续使用已有组件。

默认应用镜像、健康检查、宿主机入口和 systemd 单元使用 Rust。旧 Python 实现可从 Git 历史的已部署版本恢复，不再作为运行时依赖。源码入口切换不代表生产服务器已部署；生产升级和回退必须单独验收。

## 构建和验证

```sh
cargo build --locked --release
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets
node --test tests/test_storage_ui.js
docker build -t open-assets:local .
```

运行时镜像只包含 Rust 二进制和 Git、SSH、CA、OpenSSL 系统工具，不包含 Python。服务示例：

```sh
target/release/open-assets --config private/repositories.yaml validate
target/release/open-assets --help
docker compose config --quiet
```

容器使用现有容器路径配置；宿主机管理使用 `open-assets operator --root /app/open-assets …`，不得直接把宿主机配置用于容器发布路径。

## 数据和安全兼容

- 保留公开 URL、仓库 key、任务 ID、SHA、队列和状态目录的格式。
- 保留 references-v1 清单与 V3 增量证明，并兼容 Python 默认 JSON 的摘要字节；Unicode 转义和分隔空格都有固定对照样本。
- 保留完整路径预检、SVG 限制、HMAC 通知校验、Git 资源限制和来源对象核对。
- 未变文件使用只读共享 inode；对象引用不变成逐版本的对象复制。预备快照仍只是目录及硬链接视图。
- 保留单发布锁、激活前远端分支复核、原子切换、前后校验，以及失败后的旧指针恢复。
- 清理默认预览，保留当前版本、固定版本、活动任务、回退记录、引用对象和恢复证据保护；恢复与审计不绕过校验。
- Info 容器保持只读，不挂载 Docker socket；宿主机采集和权限管理保持独立。

Rust 严格拒绝 `schema_version: true`。旧 Python 的数值相等比较曾把该非法布尔值视为 `1`；这三个恶意样本的差异在兼容测试中明确标注，不影响合法旧记录。

## 独立全栈门禁

`rust/examples/fresh_stack.rs` 只允许显式启用的空 Linux Docker 环境。它拒绝已有 `/app/open-assets/private`、`/data/asset-stack` 或任何已有容器的 Docker daemon。不得为运行测试清空用户的 Docker 环境。

该验收程序使用 Rust CLI 创建配置、密钥和受限身份，再通过真实 SSH Push 验证签名 Webhook、发布、对象引用、共享 inode、恶意 SVG 拒绝、回退和恢复核对。它还检查 PNG/SVG 响应、安全头、HTTPS 入口、管理入口认证及运行镜像中不存在 Python。所有凭据只留在一次性环境的 `private/`，不得上传为 CI artifact。

## 现网切换边界

现网部署必须另行完成：记录每个角色的旧镜像，备份源码和受保护配置，等待活动任务结束，再切换应用角色与宿主机二进制。不得运行初始化覆盖已有配置，不迁移或重建 ACME，也不因为语言迁移修改 Nginx 路由。回退必须同时恢复旧角色镜像、旧二进制和原启动入口，保留业务数据与新旧版本清单。

语言重写不能保证真实端到端达到三秒。Git 推送和远端应答仍需独立测量；只比较 Publisher 内部耗时不能作为端到端结论。
