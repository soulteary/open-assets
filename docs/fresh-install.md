# 从零部署测试

使用独立 Linux VM 或独立 Docker daemon，按[端到端测试说明](../tests/e2e/README.md)运行 `fresh-stack.sh`。脚本会创建账号、凭据、仓库、对象和网络，不能对现有服务执行。

测试包含首次准备和初始化、拒绝重复初始化、新增仓库与重复接入，以及空仓库首次 Push 和后续更新。测试域名为 `acceptance.invalid`，入口使用临时 CA；公共 DNS、ACME 和云防火墙需要在实际部署中另行检查。

最近一次完整复验见[2026-11-08 从零部署记录](history/2026-11-08-fresh-install.md)，包含实际宿主 CLI、两仓库四次 Push 和隔离范围。更早结果见[首次部署归档](history/2026-11-07-fresh-install.md)；其他功能的验证见[验证范围](acceptance.md)。
