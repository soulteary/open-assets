# 开发与测试

应用、宿主机管理和验收程序均使用 Rust。浏览器界面沿用 HTML/CSS/JavaScript，测试需要支持 `node:test` 的 Node.js。使用与 CI 一致的当前稳定 Rust 工具链，记录 `rustc --version`、`cargo --version` 和 `node --version`。

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
node --check web/static/app.js
node --test tests/test_storage_ui.js
docker build -t open-assets:test .
```

普通测试使用私有临时目录、真实临时 Git 仓库和回环 HTTP 服务，不需要生产账号。测试覆盖队列预算、签名通知、Git/SVG 安全、不可变 inode、增量/预备快照、对象引用、条件删除、恢复和审计。Python JSON、浮点、MIME 与恢复规则的固定 oracle 由 Rust 测试读取，不需要 Python 解释器。

Rust 源码位于 `rust/src`，公开界面位于 `web/static`；工作区根目录维护 `Cargo.lock`。新依赖、文件格式或失败处理变更应补有意义的回归测试，不能仅断言新实现自己的内部步骤。

[独立全栈验收](../tests/e2e/README.md)使用空 Linux Docker daemon，真实创建 Gitea/OtterIO 身份和签名 Webhook，经 SSH Push 验证发布、拒绝危险 SVG、共享 inode、回退、缺失对象和本地快照恢复，以及 HTTPS/管理认证。不要为运行测试清空已有 Docker 环境，也不要挂载生产 private、数据或 socket。

迁移范围、严格校验差异和生产切换边界见 [Rust 迁移说明](rust-migration.md)。历史 Python 验收留在 [历史记录](history/README.md) 与 Git 历史中，不能当作本次 Rust 验收结果。
