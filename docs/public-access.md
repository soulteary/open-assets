# 公开静态资源入口

此策略只覆盖图片域名和公开发布对象。Git、Info、认证和存储管理入口保持各自认证与压缩策略。

## 配置

复制 `config/public-access.example.json` 为受保护的 `private/public-access.json`，把示例白名单替换为自己的站点。开源示例仅使用占位域名。不要将实际站点白名单提交到仓库。

```sh
bin/open-assets --config private/repositories.yaml render private/generated \
  --public-access-config private/public-access.json
bin/open-assets configure-edge --reference-releases \
  --domains private/domains.json --ca private/tls/ca.crt \
  --public-access-config private/public-access.json \
  --output /path/to/existing/traefik/dynamic/open-assets.yaml
```

先核对并备份现网配置，再进行 Nginx 配置检查和重载、Reader 更新及 Traefik 动态配置更新。已有自定义路由不能直接用生成文件覆盖；仅合并公开入口变更。Reader 通过只读挂载的 `/config/public-access.json` 加载策略，修改压缩或防盗链策略后需重启 Reader；修改限流策略后，先重新 render 再重建唯一的 public-limiter 容器。不要仅原子替换宿主文件后 restart，因为文件 bind mount 可能仍指向旧 inode。限流切换应核对容器内策略哈希并保留旧配置、旧镜像；回退先恢复旧路由，再恢复旧策略与镜像。

使用独立策略文件可以避免改变 `repositories.yaml` 的发布配置版本，从而保留已验证对象和本地快照的增量复用。`render` 还会原子写出 `private/generated/public-access.json`，供 Reader 挂载。

## 行为

- gzip 只用于 `js`、`html`、`css`、`xml`、`json`、`shtml`、`htm` 后缀，不区分大小写；默认包含 256 字节与 2 MiB 两个边界。上下限可收窄，不能扩大。已有编码、Range/Content-Range、错误响应及客户端不接受 gzip 时不压缩。采用流式处理，不保存压缩文件副本；合并 Vary 并保持条件请求与 HEAD 语义。
- HTTPS 公开资源路径仅允许 GET/HEAD，其他方法返回 405 并关闭连接，不向对象源转发请求体、Cookie 或 Authorization。HTTP 公开入口直接返回 302，保留路径与查询，不读取 POST 内容。302 必须包含 Location 等协议必要响应头；不回显请求头，不设置 Cookie。
- 公开响应删除 Strict-Transport-Security；这不能撤回客户端已经缓存的 HSTS 或浏览器预加载项。
- 防盗链支持精确主机、`*.example.com` 子域及空 Referer。通配符不包含根域；需要根域时另列。禁止 userinfo、非法协议、反斜线和伪造主机后缀。Referer 不是身份认证，允许空 Referer 的客户端仍可直接下载。
- 限流由单实例 `public-limiter` 原子检查并扣款，HTTP/HTTPS、图片/OSS 跨路由共享预算：图片每 IP 平均 50/秒（突发 100）、OSS 每 IP 平均 20/秒（突发 8）、所有公开路由合计平均 100/秒（突发 100）、其中 OSS 合计平均 40/秒（突发 12）。返回 429 和 Retry-After: 1；被拒绝请求不消耗其他预算。令牌桶允许突发，并非任意滚动一秒严格最多 100/40 次；任意长度 t 秒内接受数量不超过 burst + average × t。
- 新配置使用 `rate_limits`，每个预算分别设置 `average` 和 `burst`；average 范围 1–100000，burst 范围 1–10×average。旧 `qps` 仍可读取，作为两个每 IP 预算的附加上限；不会关闭全站/OSS 总量保护。新示例不再使用该字段。
- `public-limiter` 必须只运行一个实例，不可直接横向扩容，否则不再是共享总量。进程重启会重置内存桶至突发额度。它仅挂载策略，不接触 Git、S3 凭据或业务数据；64 MiB、0.25 CPU、32 进程预算。限流服务不可用时入口拒绝请求，不绕过限流。
- Traefik ForwardAuth 明确关闭 trustForwardHeader 和 forwardBody，仅将原始连接 IP 传入固定角色地址；Cookie、Authorization、请求体及客户端伪造 X-Forwarded-For 不传给限流服务。HTTP/HTTPS 同一 IP 共用预算，IP 端口和转发头不能创建新身份。前置 CDN 时需另行设计可信真实 IP 方案。
- IP 状态最多 16384 项，空闲 60 秒回收；满载时拒绝新身份，不重置活跃 IP 预算。内部 HTTP 使用成熟协议解析器，有连接、头部大小及超时限制。管理入口不接入这些公开限流预算。

## 内部端口与重定向

Nginx 8080 提供公开图片；文本后缀经 Reader 8085 回源 Nginx 8086，精确执行压缩大小限制，其余图片保持直接静态服务。Nginx 8084 为公开对象做前置校验，再访问 Reader 8083。8086 不应用文本代理，避免循环；8087 只返回 HTTPS 302。两条 Reader 代理路径共享 16 个并发连接预算，超额返回 503，避免双监听端口耗尽 Reader 线程；静态图片直出不占用此预算。额外的 public-limiter 8088 仅接受来自入口的内部准入请求；健康和不含 IP 列表的计数接口也只在内部可达，不发布到宿主机。上述端口只在容器网络内使用，不应发布为宿主机公网端口。

若现有 Traefik 在 web entrypoint 上配置全局 HTTP 跳转，将其 `http.redirections.entryPoint.priority` 设为 1，让优先级 100 的公开 HTTP 路由生效；管理域名仍使用既有全局跳转。不要将 `permanent: false` 当作所有方法都返回 302 的保证。ACME 的账户、证书及存储挂载保持原样，同一时刻只有一个入口实例写入。

## 可选 HTTP/3

默认不启用 QUIC。确认 Traefik 版本支持并可实际从公网验证 UDP 后，可在现有 `websecure` entrypoint 增加 `http3: {advertisedPort: 443}`，并在同一 Traefik 服务增加 `443:8443/udp` 映射，放行宿主和云防火墙 UDP 443。保留 TCP 443；不要为 HTTP/3 再启动一个 ACME 实例。共享该 entrypoint 的管理路由也会获得 HTTP/3 支持；仅看到 Alt-Svc 不能证明公网 QUIC 已可用。
