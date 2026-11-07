# 从零部署验收

## 大文件与资源预算专项

`resources.py` 必须显式设置 `OPEN_ASSETS_RESOURCES_TEST=1`，只连接内部测试网络的 `http://objects-fixture:9000`，使用独立的 `resource-fixture` 桶。运行前创建可写临时目录并通过 `TMPDIR` 指向它，预留至少 1 GiB 磁盘；提供该临时 OtterIO 的凭据文件，不挂载生产状态、网络或凭据。

专项在限制为 384 MiB 内存的测试容器（源端 Git 打包与 Publisher 在同一容器）内，生成并提交 96 MiB 不可压缩文件，完成真实 Git fetch、首次发布、已有对象的重复发布和完整分块读回，检查图片 current、队列满时的新任务拒绝及重复事件去重。它要求 Publisher 自身峰值 RSS 小于单文件字节数，并输出数值；该值不包含 Git 子进程、存储容器或宿主机缓存，不是部署总内存。测试数据应使用磁盘目录，不放到容器同等大小的 tmpfs 中。

普通单元测试还验证并发准入、归档/隔离记录预算、Git 超时、实际超限拉取后 source 引用不变，以及过长/截短对象响应的关闭与拒绝。小容量测试 tmpfs 显式降低 `git_min_free_bytes`，不改变生产默认值。专项完成后仅删除此次测试容器、网络和临时目录，不做全局 prune。

这里的脚本在空的独立 Docker daemon 上创建完整测试栈。它会创建账号、密钥、仓库、网络和文件，不属于普通单元测试，不应对已有部署运行。

## 环境

在可丢弃的 Linux VM 或 Docker-in-Docker 容器内放置源码，路径为 `/app/open-assets`。安装 Docker Engine/Compose、Python 3、PyYAML、bcrypt、Git、OpenSSH 客户端、OpenSSL 和 curl。需要能拉取 .env.example 中的镜像，以及模板固定的 Traefik 镜像，并可下载固定版本的 Certs Maker 官方发行包。

只把源码复制进去，不复制真实 private、.env、SSH 配置、ACME 状态或云凭据。测试 Docker daemon 不应发布远程 API，不要挂载生产或宿主机 Docker socket。容器内运行 DinD 需要额外权限，应把它限制在专用测试环境中。

以下命令必须在这个环境内执行：

```sh
cd /app/open-assets
OPEN_ASSETS_FRESH_TEST=1 sh tests/e2e/fresh-stack.sh
```

脚本拒绝已有 private、数据目录或容器。该检查不能自动证明你的 Docker context 是正确的，运行前仍需确认连接的是专用 daemon。正常部署目录不能靠删除检查文件来满足前提。

## 覆盖范围

流程按部署文档执行 prepare-private、镜像构建、后端就绪检查、initialize-services、Nginx 配置生成和发布服务启动。随后：

- CLI 新建 content/e2e-images，再次执行，检查仓库 ID、Key/Hook 数量和 Worker 启动时间。
- 请求冲突前缀，确认没有创建对应仓库。
- 核对重复执行准备/初始化会被拒绝，凭据与配置哈希不变。
- 使用个人 SSH 密钥克隆两个空仓库，执行使用指南中的首次提交和后续更新，各完成两次真实 Push。
- 等待两个签名事件对应任务达到 published。
- 用独立测试 Traefik 检查图片、不可变附件、中文/特殊字符路径、Range 和管理入口认证。

测试域名全部位于保留的 acceptance.invalid 下。入口使用临时 CA 签发的文件证书，路由基于 configure-edge.py 生成，但测试副本移除 ACME resolver。它不验证真实 DNS、云安全组、公共证书申请或浏览器 UI；这些范围与单机生产验收记录分开。

结果写在 `private/e2e/result.json`，诊断文件和生成凭据也只在私有测试目录。公开保存结果前需检查内容，不上传整个 private。

## 退出与清理

测试不自动删除环境，以便检查失败。完成后销毁专用 VM/容器及其独立数据卷即可。不要在不确定 Docker context 时使用全局 prune 或批量删除命令。

## 独立对象统计测试

objects.py 只用于专用临时网络中的 OtterIO，目标固定为 http://objects-fixture:9000 和 statistics-fixture 桶，必须设置 OPEN_ASSETS_OBJECTS_TEST=1。用临时凭据文件提供 S3_ACCESS_KEY_FILE、S3_SECRET_KEY_FILE；测试容器需能导入候选 open_assets 模块。不要连接生产网络、挂载生产 private 或发布服务端口。

脚本创建 1003 个带中文和特殊字符的主前缀对象、2 个相邻前缀对象、空前缀及范围外对象，检查数量和字节总和。每轮限制 1 页，验证跨轮续采；只在测试内调整尝试时间以模拟间隔，最后确认间隔内重复执行没有列表调用。测试完成后只删除专用测试容器和网络，不执行全局 prune。

## 对象历史清理

object-retention.py 需要 OPEN_ASSETS_OBJECT_RETENTION_TEST=1，仅使用 http://objects-fixture:9000 和 retention-fixture 桶。提供临时 root 与受限清理账号的凭据文件；清理账号只授权该测试桶的 assets/releases 前缀，按 object_retention_setup.iam_policy 生成。脚本验证默认预览、显式应用、重复执行、保护对象内容和越权拒绝，并实测错误 If-Match 的行为。测试时间只通过内部函数参数注入，CLI 没有绕过年龄的选项。所有对象删除仅作用于可销毁的测试实例。

## 对象恢复

object-restore.py 需要 OPEN_ASSETS_OBJECT_RESTORE_TEST=1，固定目标为 http://objects-fixture:9000 和 restore-fixture 桶。使用临时 root 准备并删除测试对象，另给恢复账号 ListBucket/GetObject/PutObject，明确不授予 DeleteObject。脚本用真实 Git 历史与 OtterIO 验证已完成/归档任务不阻止恢复，原任务字节和图片入口不变，以及重复执行、上传中断重试和同字节元数据修复。测试只连接可销毁实例，不挂载生产发布状态。

本地恢复验收还删除目标版本的两份本地副本，验证 `--with-local` 重建、重复执行不重复安装且不切换当前版本，随后验证 `--with-local --rollback` 显式切换及原任务字节不变。

对象恢复隔离验收同时调用主机只读摘要，核对真实恢复报告中的对象成功、部分上传失败、本地校验和显式回滚标志；不向业务状态注入失败报告。

恢复状态核对验收使用同一可销毁 Git/OtterIO 环境：部分上传失败时保留删除屏障；切换阶段故障产生实际 activation_uncertain 报告后，只读核对验证对象、本地副本和原引用，同时保留原报告字节；正常显式回滚后再次核对目标引用及 Info 过滤摘要。不会向业务报告制造未结案状态。

结案隔离验收在部分上传失败时确认拒绝且屏障不变；在实际切换故障产生不确定报告后，同锁重新核对并显式结案，原报告/current/job 字节保持不变；随后正常恢复/回滚会触发 Info 的原证据变化提示，既有审计快照仍逐字节保留。业务报告不注入故障。

审计归档隔离验收对真实恢复/结案证据执行 SHA 查询、预览、显式归档和独立校验，确认压缩包保存所有原始字节，最新在线结案、未覆盖的不确定证据和 current/published/job 保持保护。单元测试另覆盖超出 Info 上限后的归档恢复、中断清理、坏包、链接和展开限制。

审计看板隔离验收核对真实归档后的在线/压缩包/暂存数量与实际执行摘要，并在缓存有效期内禁止再次调用扫描函数，确认重复采样不遍历归档。界面测试覆盖 tab 鼠标/键盘切换、刷新保持选择、缺失/失败/旧数据以及部分归档结果。

大文件专项还对实际 Git 缓存与两个发布任务执行资源采集，将限额拒绝产生的固定事件交给 Info 摘要，检查全局计数、Git 字节和满额提示。随后禁止所有目录测量函数，确认间隔内采集和多次摘要读取仍成功；隔离环境不向业务队列注入满额事件。
