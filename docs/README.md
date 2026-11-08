# 文档目录

先完成一次发布，再按需要配置监控、保留和恢复。示例使用 `lab.dev`、`node-name`，请替换为自己的域名和节点名；私有配置和运行记录不要提交到仓库。

## 第一次使用

- [部署](deployment.md)：准备服务器、初始化服务、接入现有 Traefik。
- [使用指南](usage.md)：登录、添加个人 SSH 密钥、空仓库首次发布、更新和获取链接。
- [仓库接入](cli.md)：新增或重复接入白名单仓库。

## 日常操作

- [命令速查](operator.md)：查询状态、重试、生成链接及选择恢复命令。
- [配置说明](configuration.md)、[仓库与 URL](repositories.md)：配置位置与映射规则。
- [Info](info.md)：安装采集服务，阅读健康、容量和恢复结果。
- [部署诊断](doctor.md)：检查必需/可选定时器、最近执行结果和缓存时效。
- [故障排查](troubleshooting.md)：按现象检查发布和管理入口。

## 运行维护

- [维护与备份](operations.md)、[升级与证书](upgrade.md)、[日志](logging.md)。
- [健康检查](health.md)、[资源预算](resource-limits.md)、[本地历史保留](retention.md)、[对象保留](object-retention.md)。
- [恢复内容](object-restore.md)、[核对恢复](restore-check.md)、[恢复结案](restore-close.md)、[审计归档](restore-audit.md)。
- [人工确认拒绝提醒](closure-acknowledgment.md)：保存审核原因与证据，不替恢复结案。
- [手工接入](manual-onboarding.md)：仅用于需要检查每一步的定制部署。

## 开发与参考

- [开发测试](development.md)、[发布流程](architecture.md)、[性能与缓存](performance.md)。
- [使用限制](usability.md)、[风险与控制](risks.md)、[安全说明](../SECURITY.md)。
- [当前验证范围](acceptance.md)、[历史验收归档](history/README.md)。历史环境与测试数量不代表当前版本重新执行过同一套验证。
