# 文档目录

第一次使用只需完成部署和一次分享；清理、恢复、结案与审计在需要时再看。示例使用 `lab.dev`、`example.org` 和 `node-name`，真实配置、凭据和运行记录只保存在私有环境。

## 首次使用

1. 阅读[环境要求](requirements.md)，准备服务器、域名和个人电脑工具。
2. 没有入口服务：走[全新服务器部署](new-server.md)；已有 Traefik：走[现有入口部署](deployment.md)。只选择一条，不重复初始化。
3. 按[首次分享文件](first-share.md)完成上传/Push、确认结果和匿名下载。选择网页上传时，管理员先配置[独立公开目录](web-uploads.md)。
4. 安装 [Info 采集](info.md)与[日志轮转](logging.md)，按[维护说明](operations.md)安排备份。

## 日常操作

- [使用指南](usage.md)：登录、网页管理、个人 SSH 密钥、Git 更新和获取链接。
- [CLI](cli.md)、[命令速查](operator.md)：查询 key/任务、接入其他仓库、重试和生成链接。
- [配置说明](configuration.md)、[仓库与 URL](repositories.md)：配置位置、修改生效和路径规则。
- [部署诊断](doctor.md)、[故障排查](troubleshooting.md)：按失败项和操作阶段检查。
- [Info](info.md)：读取健康、容量、证书和缓存状态；页面不执行发布或删除。
- [维护与备份](operations.md)、[组件升级与证书](upgrade.md)、[日志](logging.md)。

## 高级维护

以下不是首次分享的必需步骤。先核对目标、备份和保护条件，页面提示不等于授权删除或回滚。

- [健康恢复](health.md)、[Git/队列资源预算](resource-limits.md)。
- [本地版本保留](retention.md)、[对象保留](object-retention.md)：分别管理本地与 S3 历史，实际删除可能使旧链接失效。
- [恢复内容](object-restore.md)、[核对恢复](restore-check.md)、[恢复结案](restore-close.md)：默认恢复不切换版本，核对不替结案。
- [审计查询与归档](restore-audit.md)、[人工确认拒绝提醒](closure-acknowledgment.md)。
- [手工接入](manual-onboarding.md)：用于理解和定制各步骤，日常仓库接入优先使用 CLI。

## 开发与参考

- [参与贡献](../CONTRIBUTING.md)、[开发测试](development.md)。
- [架构与发布流程](architecture.md)、[性能与缓存](performance.md)。
- [使用限制](usability.md)、[风险与控制](risks.md)、[安全说明](../SECURITY.md)。
- [验证记录](acceptance.md)、[历史验收索引](history/README.md)。记录只适用于其中写明的提交、环境和范围，不能证明后续版本已重新验证。

- [SVG 使用范围](svg.md)：固定图片入口、版本附件与网页上传的差异。
- [发布与保留修订](review-fixes.md)：本轮改动、升级步骤与保留边界。
