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

先核对并备份现网配置，再进行 Nginx 配置检查和重载、Reader 更新及 Traefik 动态配置更新。已有自定义路由不能直接用生成文件覆盖；仅合并公开入口变更。Reader 通过只读挂载的 `/config/public-access.json` 加载策略，修改压缩或防盗链策略后需重启 Reader。

使用独立策略文件可以避免改变 `repositories.yaml` 的发布配置版本，从而保留已验证对象和本地快照的增量复用。`render` 还会原子写出 `private/generated/public-access.json`，供 Reader 挂载。

## 行为

- gzip 只用于 `js`、`html`、`css`、`xml`、`json`、`shtml`、`htm` 后缀，不区分大小写；默认包含 256 字节与 2 MiB 两个边界。上下限可收窄，不能扩大。已有编码、Range/Content-Range、错误响应及客户端不接受 gzip 时不压缩。采用流式处理，不保存压缩文件副本；合并 Vary 并保持条件请求与 HEAD 语义。
- HTTPS 公开入口仅允许 GET/HEAD，其他方法返回 405 并关闭连接，不向对象源转发请求体、Cookie 或 Authorization。HTTP 公开入口直接返回 302，保留路径与查询，不读取 POST 内容。302 必须包含 Location 等协议必要响应头；不回显请求头，不设置 Cookie。
- 公开响应删除 Strict-Transport-Security；这不能撤回客户端已经缓存的 HSTS 或浏览器预加载项。
- 防盗链支持精确主机、`*.example.com` 子域及空 Referer。通配符不包含根域；需要根域时另列。禁止 userinfo、非法协议、反斜线和伪造主机后缀。Referer 不是身份认证，允许空 Referer 的客户端仍可直接下载。
- `qps` 默认 100，范围 1–100000。每个公开入口按实际连接 IP 分别计数，令牌桶平均为 qps/秒，瞬时 burst 同为 qps，超额返回 429；不同域名/HTTP 路由不共用一个全局桶。默认不相信客户端伪造的 X-Forwarded-For。前置 CDN 时需明确配置受信代理及真实客户端地址，不可直接信任任意转发头。

## 内部端口与重定向

Nginx 8080 提供公开图片；文本后缀经 Reader 8085 回源 Nginx 8086，精确执行压缩大小限制，其余图片保持直接静态服务。Nginx 8084 为公开对象做前置校验，再访问 Reader 8083。8086 不应用文本代理，避免循环；8087 只返回 HTTPS 302。上述端口只在容器网络内使用，不应发布为宿主机公网端口。

若现有 Traefik 在 web entrypoint 上配置全局 HTTP 跳转，将其 `http.redirections.entryPoint.priority` 设为 1，让优先级 100 的公开 HTTP 路由生效；管理域名仍使用既有全局跳转。不要将 `permanent: false` 当作所有方法都返回 302 的保证。ACME 的账户、证书及存储挂载保持原样，同一时刻只有一个入口实例写入。

## 可选 HTTP/3

默认不启用 QUIC。确认 Traefik 版本支持并可实际从公网验证 UDP 后，可在现有 `websecure` entrypoint 增加 `http3: {advertisedPort: 443}`，并在同一 Traefik 服务增加 `443:8443/udp` 映射，放行宿主和云防火墙 UDP 443。保留 TCP 443；不要为 HTTP/3 再启动一个 ACME 实例。共享该 entrypoint 的管理路由也会获得 HTTP/3 支持；仅看到 Alt-Svc 不能证明公网 QUIC 已可用。
