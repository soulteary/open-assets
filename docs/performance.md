# 资源采集、限量压测与缓存选择

## 如何测量

性能数据必须注明场景、样本数、并发、请求速率上限和耗时。Docker CPU 的 100% 表示一个逻辑核；主机 CPU 的 100% 表示整机。Docker 内存工作集、进程 RSS、主机可用内存与 buff/cache 不是同一个指标，不能互相替代或直接相加。

本项目提供宿主机只读采样与限量读取测试。在可信管理员 shell 执行：

```sh
cd /app/open-assets
python3 -m open_assets.benchmark --baseline --seconds 20
```

资源原始采样写入 `private/performance/baseline.json`。`info` 定时采集程序也会持续提供 CPU、内存和容量快照，但不是长期时序数据库。

读取测试需要一个已经发布、同时包含图片与普通附件的专用测试仓库。不要选取生产大文件或包含秘密的内容。使用 repo ensure 接入、个人 SSH Key Push 并等待 published 后，再执行：

```sh
python3 -m open_assets.benchmark \
  --key performance-check \
  --seconds 10 \
  --max-rps 40 \
  --concurrency 1 4 8
```

测试通过回环地址访问真实 HTTPS 入口，保留 SNI、证书验证与 Host，经 Traefik 进入 Nginx/OtterIO。每个 Worker 复用自己的连接，每个响应都核对状态码、字节数上限及 SHA-256。只读图片与版本附件，不进行删除或批量写入；输入文件限制在 16 MiB 以内。

每场景目标持续时间最多 30 秒（已发出的请求还需等待其超时或完成）、并发最多 8、总请求速率最多 100 req/s，默认 40 req/s。启动和运行中检查可用内存、磁盘与系统负载；低于 512 MiB 可用内存、低于 2 GiB 空闲盘或一分钟负载超过逻辑核数的两倍时终止。累计错误会提前停止。保护检查不是生产压测的完整隔离措施，执行时仍应观察已有服务。

结果保存到 `private/performance/latest-summary.json`，包含请求数、错误数、吞吐、p50/p95/p99 和资源采样。Info 只读取其中明确的摘要字段。该测试有请求速率上限；达到上限不表示达到服务器容量上限。客户端与服务共享 CPU，回环测试不覆盖公网带宽、用户网络、跨区域延迟和 CDN。不要据此承诺公网并发容量。

## Nginx 的优化边界

Nginx 当前负责本地图片读取，TLS 在 Traefik 终止，附件仍由 OtterIO 提供。应分开观察这三个组件的 CPU 与延迟，不能把所有读取延迟归因于 Nginx。

可以测试的低复杂度选项是 `sendfile on`、适量 Worker、连接复用及适合实际并发的 worker_connections。它们可能降低用户态拷贝或减少连接开销，但低负载、热缓存、小文件场景不一定有可测收益。对比时保留相同文件、并发、速率上限和缓存条件，记录字节、Range 与图片切换是否正确；不要仅报告“优化后更快”。选项含义见 [Nginx core module](https://nginx.org/en/docs/http/ngx_http_core_module.html) 与 [core 配置](https://nginx.org/en/docs/ngx_core_module.html)。

当前保留 `open_file_cache off`。固定图片 URL 经过 current 软链接切换，缓存文件描述符或元数据可能继续使用旧版本；不能为了减少 stat 就牺牲切换语义。`sendfile` 与内核页缓存不等同于 open_file_cache。

PNG/JPEG/WebP/AVIF、压缩包等通常已有压缩，不默认对它们再次 gzip。固定图片 URL 继续使用短缓存与重新验证；只有带 SHA 的不可变附件适合长缓存。对同名图片要求立即更新时，应采用版本 URL 或更严格的重新验证策略，而不是把固定 URL 标成 immutable。

## 是否需要 Redis、Memcached 或其他内存缓存

是否增加缓存，应先采样目标机器的可用内存、读取延迟和流量。项目默认不增加 Redis/Memcached；内核页缓存和 HTTP 缓存已承担一部分重复读取。[历史采样](history/2026-11-07-performance.md)仅供比较测量口径。

当前不增加独立内存缓存服务。主要依据是数据规模小、内存余量充足、已有 Linux 页缓存与 HTTP 缓存，并未观察到必须由独立缓存解决的内存或后端瓶颈。MemAvailable 是内核对可供新进程使用内存的估算，不能再把全部 buff/cache 加到它上面。页缓存机制见 [Linux 内存概念](https://docs.kernel.org/admin-guide/mm/concepts.html)。

Redis/Memcached 不会在启动后自动加速 Nginx 文件或 S3 对象。接入还需要缓存协议、对象序列化或模块、容量与淘汰策略、身份边界，以及固定图片 URL 的更新/回滚失效机制；二进制文件还会增加重复数据与复制开销。Redis 作为多实例会话存储又是另一个用途，不能把会话存储和资源缓存混为一谈。

后续若持续观测到重复对象读取使 OtterIO CPU 或磁盘成为瓶颈，可先评估公开、不可变附件的 CDN 或带容量上限的反向代理缓存；管理接口、带身份请求和未完成版本不应混入公开缓存。涉及代理缓存时另测 Range/206、Cache-Control、错误响应及缓存键，不能直接照搬图片配置。若数据热集显著超过页缓存，再比较缓存命中率、后端减载与额外内存的收益。

生产方案应给 Git、发布 Worker、存储、系统页缓存和构建/升级留出余量，不以“还有空闲内存”为理由启动一个没有明确命中率和失效策略的缓存组件。

## 当前 Nginx 配置

配方使用自动 Worker、1024 个 worker_connections、sendfile、tcp_nopush、tcp_nodelay 和 30 秒 keepalive，保持 open_file_cache 关闭。更改后应检查图片切换、历史附件、Range 与完整字节；用相同样本对比延迟和资源消耗。历史样本及结果见[采样记录](history/2026-11-07-performance.md)。
