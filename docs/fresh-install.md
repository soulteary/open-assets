# 隔离环境从零部署记录

日期：2026-11-07。被测应用代码基线为 `ce17cb9c0e87b649ad21081dcd4feb321f3cce8b`。本次新增的端到端脚本和记录不改变初始化、接入或发布程序的运行逻辑。

## 隔离方式

在本机 Podman 的 Linux VM 中使用两个先后运行的 Docker-in-Docker 实例。每个实例有独立 Docker daemon、配置、数据和网络；没有挂载宿主机目录或生产 Docker socket，也没有发布宿主机端口。第二个实例的 daemon 只监听自己的 Unix socket。

第一次按文档逐步执行，第二次用新的空实例执行 `tests/e2e/fresh-stack.sh`。第二次执行前没有应用容器、private 配置或 `/data/asset-stack` 数据。没有访问原部署主机，也没有复制其 DNS、ACME、SSH 或应用凭据。

本机 Podman 后端只允许一个 VM 运行，因此没有停用现有 VM 去启动另一台 VM；测试使用专用 DinD 实例完成。这个限制属于测试工具环境，不是 Open Assets 的部署要求。

实测环境：Linux aarch64，Docker Engine 28.5.2，Docker Compose 2.40.3，宿主机脚本 Python 3.12.16，Publisher 镜像 Python 3.13.18。应用镜像使用 .env.example 中固定的摘要。

## 执行与结果

两轮均完成以下流程，第二轮完整脚本退出码为 0：

1. 从源码生成全新的密码、内部 CA、SSH 密钥和私有配置。
2. 拉取固定镜像、构建 Publisher，启动 Gitea、OtterIO、OC 和 Stargate，等待后端就绪。
3. 执行 initialize-services.py，创建初始 content/images 仓库、存储桶及身份，生成 Nginx 配置，启动接收器和 Worker。
4. 执行 repo ensure 接入 content/e2e-images。重复执行后，仓库 ID 不变、匹配的只读 Key 和 Hook 各一个、Worker StartedAt 不变。
5. 请求已占用的根前缀，命令拒绝，Gitea 中未创建对应冲突仓库。
6. 使用新生成的个人 SSH 密钥克隆初始仓库和新增仓库，分别 Push 图片与文档；两个任务均达到 published。
7. 通过独立测试 Traefik 读取两组图片和不可变附件，逐字节比对；中文、空格、加号、百分号路径及图片 Range 请求通过。
8. 匿名管理请求跳转到 Stargate，登录后可读取 Traefik JSON API。

测试入口使用临时 CA 的文件证书。路由由 configure-edge.py 生成，测试副本去掉公共 ACME resolver；没有调用公共 DNS 或 ACME 服务。

## 结论与范围

在上述环境中，文档所列初始化、接入和发布顺序可执行。没有发现需要修改这些程序逻辑的问题。本次补充端到端脚本，避免后续只依靠手工操作记录；另有两个单元测试确认脚本在没有明确 opt-in 时拒绝执行。

这份记录不覆盖公共 DNS/证书签发、云安全组、真实浏览器、压力测试、升级降级和所有中断恢复场景。已有的浏览器与单机部署记录见 [验收文档](acceptance.md)，不能把不同环境的结果合并为一次全平台验证。

复现步骤和脚本的使用条件见 [端到端测试说明](../tests/e2e/README.md)。普通 CI 只运行单元/集成测试和构建，不会自动执行这套需要空 Docker daemon 的部署流程。
