# 开发与测试

应用使用 Python 3.13。宿主机脚本还需要系统 Python 的 PyYAML、bcrypt、Git、OpenSSH 和 OpenSSL；部署依赖见[部署说明](deployment.md)。

```sh
python3 -m venv .venv
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

CI 执行格式检查、Python/界面测试和镜像构建；不会自动运行需要独立 Docker daemon 的从零部署测试。变更初始化、权限或路由后，应另外运行相关端到端验收。测试结果记录版本、环境和范围，放在[验证记录](acceptance.md)或[历史归档](history/README.md)。
