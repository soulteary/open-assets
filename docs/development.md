# 开发与测试

先看[环境要求](requirements.md#开发与测试)和[贡献指南](../CONTRIBUTING.md)。应用与常规 Python 测试使用 Python 3.13，界面测试另需支持 `node:test` 的 Node.js；先记录 `python3 --version` 和 `node --version`。宿主部署脚本的依赖与容器内依赖分开安装，见[环境要求](requirements.md)。

```sh
python3.13 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
pip install ruff==0.14.4
ruff check open_assets scripts tests
ruff format --check open_assets scripts tests
python -m unittest discover -s tests -v
node --check open_assets/static/app.js
node --test tests/test_storage_ui.js
```

普通测试使用临时目录、Git 仓库和回环 HTTP 服务，不连接生产数据。首次发布测试直接执行使用指南中的 shell 命令，覆盖空仓库与后续更新；初始化测试检查拒绝已有、并发和中断状态。

[端到端脚本](../tests/e2e/README.md)需要可丢弃的 Linux 环境。它会初始化真实 Gitea/OtterIO，并验证仓库接入、SSH Push、公开读取和管理入口。不要挂载生产 Docker socket、private 或数据。对象清理与恢复的专项只能连接专用测试存储。

CI 先执行格式检查、Python/界面测试和镜像构建，再在独立 GitHub 托管 runner 上执行从零部署与网页分享 API 验收。变更初始化、权限或路由后，应另外运行相关端到端验收。测试结果记录版本、环境和范围，放在[验证记录](acceptance.md)或[历史归档](history/README.md)。
