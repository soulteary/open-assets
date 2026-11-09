# 接入仓库与权限协调

当前实现使用 Rust 管理 CLI，接入流程统一协调真实仓库 ID、只读 Deploy Key、签名 Webhook、S3 仓库前缀、配置备份和图片映射，避免分步手工修改遗漏权限或覆盖已有路由。

在管理员 shell 暂停该仓库的 Push，先查询现有配置，再接入独立路径：

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo list --json
sudo ./scripts/open-assets repo ensure content/project-b --key project-b --mount-path /project-b/
```

`key` 是后续清单、URL 和恢复记录使用的稳定标识；不能与其他仓库重复。根路径仓库不能占用已分配的图片前缀。接入完成且状态为 `ready` 后再 Push，不把 Git 接收成功等同发布成功。

CLI 会保留旧配置与 Publisher 策略，核对已有仓库、密钥和 Webhook，不通过放宽到全私网或赋予 Publisher 删除权限来解决失败。重复调用用于核对同一配置，未知或冲突状态应停止检查，不删除私钥、初始化标记或业务数据后重试。

维护者需要审阅底层请求时，可查看 `rust/src/ops/admin.rs`；不要把包含密码、API 认证头、密钥或完整私有配置的命令输出提交到 Issue。有关命令、失败恢复与配置边界，见[仓库 CLI](cli.md)、[配置说明](configuration.md)和[故障排查](troubleshooting.md)。
