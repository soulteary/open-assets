# 仓库与 URL 配置

逐步操作命令见 [实际使用指南](usage.md#6-新增白名单仓库)。

配置见 `config/repositories.example.yaml`。稳定 key 与 Gitea repository_id 共同绑定发布身份；名称用于阅读，remote 由管理员固定。仅接受配置分支 Push，禁用项拒绝入队和执行。

源目录为 public，图片子目录为 images。附件 URL：`https://oss-toh1.lab.dev/attachments/repositories/main-images/releases/<sha>/images/logo.png`；图片 URL：`https://img-toh1.lab.dev/logo.png`。

新增仓库：创建 Gitea 仓库 → 添加只读 Deploy Key → 写入 ID、key、SSH remote、分支、mount_path → validate → render → nginx -t → reload → 配置签名 Push Webhook → 首次 Push。

一个仓库使用 `/`，其他使用 `/project-a/` 等独立前缀。禁止重复或嵌套前缀。根仓库不得占用已保留的前缀。关闭发布不删除已有图片；撤下访问入口需设置 `serve: false` 并单独生成、校验、应用 Nginx 配置。

Webhook 目标为 `http://webhook:9000/hooks/publish-assets`，Content-Type application/json，事件 Push，Secret 从私有文件读取。只允许 Gitea 访问这个内部目的主机。Publisher 的 Deploy Key 不能写仓库。SSH 主机密钥必须从受信任的 Gitea 数据目录核验，不从未验证网络扫描结果直接信任。
