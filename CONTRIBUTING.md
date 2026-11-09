# 参与贡献

欢迎改进文档、报告问题、补充测试和提交修复。第一次参与可从命令示例、错误提示或文档链接开始；不需要访问维护者的服务器或取得生产凭据。

## 报告问题前

先看[使用指南](docs/usage.md)、[故障排查](docs/troubleshooting.md)和[环境要求](docs/requirements.md)。如果仍有问题，通过 [Issue 模板](https://github.com/soulteary/open-assets/issues/new/choose)选择故障、文档或功能建议。

故障报告请提供：

- 应用 commit、涉及组件版本、操作系统与 CPU 架构、Docker/Compose 版本。
- 使用新服务器路线还是接入现有 Traefik；实际执行到哪一步。
- 可复现步骤、预期结果、实际错误和已尝试的排查。
- 最少必要的脱敏日志或诊断结论；可以只说明 doctor 哪一项失败，不粘贴完整运行资料。

不要上传 `.env`、private、ACME、私钥、密码、Token、Cookie、Authorization、签名下载链接或备份。仓库 URL、域名、IP、用户名和截图也需要检查。涉及认证绕过、凭据泄露或数据破坏的问题，按[安全报告说明](SECURITY.md#报告问题)联系，不在公开 Issue 中提交细节。

## 准备开发环境

Fork 仓库并克隆自己的 Fork，创建独立分支；不要把私有部署目录作为开发工作区。环境和完整检查命令见[开发测试](docs/development.md)。

```sh
git switch -c docs/clarify-first-use
cargo build --locked
cargo test --locked
```

使用当前稳定 Rust；界面测试还需要 Node.js。测试会在临时目录创建 Git 仓库和文件，不需要生产账号。不要为了运行测试复制服务器 private 或给开发容器挂载生产 Docker socket。

## 选择与修改相符的检查

文档与命令示例：

```sh
cargo test --locked --test compatibility
git diff --check
```

Rust 代码：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Info 页面：

```sh
node --check web/static/app.js
node --test tests/test_storage_ui.js
```

Dockerfile、依赖或镜像构建相关变更另运行 `docker build -t open-assets:test .`。初始化、权限、接入、存储和路由改动应根据影响范围安排[独立端到端验证](tests/e2e/README.md)，不能用单元测试替代真实发布验收。普通贡献者无需拥有公网域名才能运行常规单元测试。

## 提交 Pull Request

让一次 PR 围绕一个具体问题，说明触发条件、修改后的行为、验证方式和未覆盖范围。需要用户调整配置时，给出升级和回退步骤；涉及公开权限、清理或恢复时，说明保护条件是否变化。

提交前检查暂存内容：

```sh
git diff --stat
git diff --cached --stat
git diff --cached --check
```

还需实际阅读暂存差异，统计和格式检查不会自动发现秘密。不要提交本地测试凭据、环境输出、截图、构建产物或新的生产域名；示例使用 `lab.dev`、`example.org`、`node-name` 等占位值。

通过 GitHub 创建 PR，使用仓库提供的模板如实填写。未执行的检查写明原因，不把历史记录当成本次验证。CI 会执行格式、Rust/界面测试、镜像构建，并在独立托管 runner 执行从零部署和网页分享 API 验收。
