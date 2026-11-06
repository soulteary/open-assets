# 部署

## 私有状态

使用独立路径保存私有运行配置和数据。首次迁移前备份旧 Compose、DNS 凭据和 ACME，校验归档且限制为 0700/0600。不要提交备份、生产日志或已渲染配置。

`private/repositories.yaml` 来自仓库示例。`private/publisher/` 保存 deploy_key、known_hosts、publisher_access、publisher_secret、ca.crt；`private/storage/` 保存 root_user、root_password。另需 webhook_secret、stargate.env 和 TLS 目录。UID：Publisher/Nginx 20010、Gitea 20001、OtterIO 20002。文件权限必须满足对应进程且不向其他用户开放秘密。

内部 CA 分别签发 storage.internal 和 Console 实际主机名证书。OtterIO /tls 使用 public.crt、private.key；OC 使用 console.crt、console.key、ca.crt。不得关闭证书验证。证书续期由运维负责，私有证书到期前重新签发并重启对应组件。

## 镜像与入口

OtterIO/OC 示例使用正式发布 manifest 摘要。其他组件首次 pull 后记录并固定实际 RepoDigest；生产不跟随 latest。保留已有 Traefik ACME resolver、账户和 DNS secret，只接入新网络与动态路由。

Traefik 管理路由必须使用 api@internal 并完整保护 /api 与 /dashboard；不得启用 api.insecure。OC 通过 HTTPS 后端连接，配置 CA 和 serverName。公开对象仅匹配 /attachments/repositories/ 且仅 GET/HEAD。Nginx 只读挂载图片父目录。

Stargate 配置 AUTH_HOST、bcrypt PASSWORDS、COOKIE_SECURE=true、精确 CALLBACK_ALLOWED_HOSTS、至少 32 字符 SESSION_EXCHANGE_SECRET、Traefik 固定地址 TRUSTED_PROXIES。不使用父域共享 Cookie。登录入口不要套 ForwardAuth。单实例重启需重新登录。

OC native 登录使用启用的独立 IAM 用户，不使用 root 或 service-account。外层 Stargate 不替代 OC 权限。必须验证代理下的登录、Cookie、CSRF、上传、下载及退出登录。

## 初始化

先启动 OtterIO 和 Gitea，创建 attachments 桶、仅允许 GetObject 的匿名前缀策略和 Publisher 专用权限。用 OC 管理命令创建 IAM 用户。创建 Gitea 管理员及私有内容仓库，关闭注册，绑定只读 Deploy Key，设置 main 分支保护和签名 Push Hook。填回实际 repository_id 后再启动 Worker。

最后验证证书、公开读取、管理拒绝匿名、SSH Push、签名事件、对象哈希和图片切换。部署时用固定源码版本；不在公共 CI 保存生产 SSH 或云凭据。

## 初始化配方

`PUBLIC_BASE=lab.dev sudo -E python3 scripts/prepare-private.py` 用于全新部署；它依赖服务器 Python 的 PyYAML/bcrypt，以及 openssl/ssh-keygen/docker。实际 base 只在私有环境传入。该配方目前针对单节点 toh1 和固定 /app/open-assets、/data/asset-stack，执行前审阅路径。已有配置不要重复执行 prepare：它会重写运行配置和重新生成 bcrypt 表达，不是现有部署的配置合并工具。

准备后构建镜像，先启动 otterio/gitea/stargate/oc，再执行 `sudo python3 scripts/initialize-services.py` 初始化固定 content/images 仓库和权限。它会使用保存在 private/ 的已有密码设置两个 IAM 用户；已有业务部署不应把该命令当作无副作用的巡检。

使用 `scripts/configure-edge.py --domains PRIVATE_DOMAINS_JSON --ca PRIVATE_CA --output DYNAMIC_DIR/open-assets.yaml` 生成路由。输出必须是 .yaml/.yml，Traefik 不会将 .json 后缀作为动态配置加载。脚本假定现有 entrypoint=websecure、resolver=le；其他环境修改后再应用。

现有 Traefik 需加入 open-assets-edge 和 open-assets-auth，后者固定地址 172.29.19.2。将这些网络写入其 Compose 声明以便重建时保留。保持原 ACME/DNS 挂载。确认 Stargate 健康后启用受保护的 api@internal，不启用 api.insecure。

三个管理域名的精确 `/_session_exchange` 路由直接交给 Stargate，不加 ForwardAuth。该接口验证一次性票据；其他管理路径全部受保护。缺少该路由会导致主机级 Cookie 登录循环。

## 浏览器认证导航落地页

将 Stargate 的 `SESSION_EXCHANGE_REDIRECT_PATH` 设为 `/__auth_return`。三个管理域名的该精确路径由独立的 Nginx 内部监听 8081 提供，并保留 `console-auth`。页面只包含固定同源 `/` 导航，不接收 callback/next 等跳转目标，响应 no-store、no-referrer 和严格 CSP。公开图片仍由 8080 提供，8081 不发布宿主机端口。

原因：浏览器会把跨站认证重定向链的 Fetch Metadata 延续到最终 OC 请求，触发 OC 的来源拒绝。一个成功加载的本地 HTML 文档结束该链，再发起同源导航。此方案不改 OC 程序，不删除或伪造 Origin/Sec-Fetch-*，不关闭 CSRF。不要用全局删除请求头替代此落地页。
