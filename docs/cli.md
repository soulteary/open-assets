# 命令行使用

命令行用于查看发布状态、接入仓库和维护历史内容。以下命令都在部署服务器的 `/app/open-assets` 执行；已进入 root shell 时可省略 `sudo`。个人电脑上的 Git 提交和 Push 见[使用指南](usage.md)。

## 先查看，再操作

```sh
cd /app/open-assets
sudo ./scripts/open-assets doctor
sudo ./scripts/open-assets repo list
sudo ./scripts/open-assets status --key main-images
```

这三条命令不修改服务。doctor 检查部署并给出下一步建议；repo list 列出仓库的稳定 key；status 查询任务。初始仓库 `content/images` 的 key 是 `main-images`，其他仓库使用列表中的实际 key。

查看单个命令的参数：

```sh
./scripts/open-assets --help
./scripts/open-assets repo ensure --help
./scripts/open-assets restore-objects --help
```

帮助不需要 Docker 或 root；实际操作仍需要相应权限。常用命令的选择见[日常操作入口](operator.md)，部署诊断结果见[doctor 说明](doctor.md)。

示例中的 `FULL_COMMIT_SHA` 是目标版本完整的 40 位小写 SHA，`JOB_ID` 是 status 输出中的完整 64 位任务 ID，执行前要替换。查询历史版本应使用那次提交的 SHA，不能直接以当前 HEAD 代替。

## 仓库接入

首次分享先看[完整例子](first-share.md)。下面用于已初始化服务器上的其他项目。

### 仓库自动接入 CLI

此命令在部署服务器宿主机上执行，自动协调 Gitea 仓库、只读 Deploy Key、签名 Push Webhook、Publisher 的 S3 前缀权限、发布白名单和 Nginx 图片映射。不需要把 Docker socket 或管理员凭据挂到应用容器里。

### 接入命令

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo ensure content/project-b \
  --key project-b \
  --mount-path /project-b/
```

如果已进入 root shell，省略 sudo。命令返回 JSON，`status: ready` 表示接入完成；它不代表仓库内容已经发布。首次创建的是空私有仓库，仍需使用个人 SSH 密钥提交 `public/images/` 和可选附件。

参数：

- `content/project-b`：Gitea 的 owner/repository。支持初始化账号 content 名下仓库，以及 content 有权限操作的现有组织。不会自动创建用户或组织。
- `--key project-b`：稳定发布身份，决定 S3 前缀和本地版本目录。绑定后不能用同一个 key 切换仓库。
- `--mount-path /project-b/`：图片 URL 前缀。只能有一个根 `/`，其他前缀不能重叠。新前缀如果遮盖根仓库当前已有目录，命令会拒绝。
- `--branch main`：可选，默认 main。命令不自动创建提交、改写分支历史或修改已有分支保护。
- `--root /app/open-assets`：全局选项，放在 repo 之前，用于指定部署根目录。该目录中的 Compose、private 与 .env 必须属于同一部署。

已有同名私有仓库可直接接入；公开或归档仓库不会被自动转换。已配置 `enabled: false` 或 `serve: false` 的项不会被命令重新启用。

### 执行前提

这是初始化之后的运维命令。需要 Linux 宿主机 Python 3、PyYAML、Docker Compose，以及完整的 private 配置：Gitea content 管理凭据、Publisher 公钥、Webhook Secret、已配置的 OC 管理 alias 与 CA、现有 assets-publish 策略和发布账号。

脚本只通过宿主机回环地址调用 Gitea API。密码从私有文件读取，不要求放在命令参数中。它不创建 root 存储账号，不重置现有密码，也不重新运行 initialize-services.py。

新增仓库或恢复中断执行时会短暂停止/重建 webhook 与 publisher；图片和附件服务继续运行。请安排发布维护窗口，看到 ready 后再 Push。窗口内旧仓库的失败 Hook 需重投或按 SHA 补发。

### 自动完成的步骤

1. 取得主机级锁，验证名称、稳定 key、URL 前缀和现有配置；前缀冲突在创建资源前拒绝。
2. 查询同名仓库，不存在时创建空私有仓库；将实际 ID 绑定到白名单。
3. 按公钥内容复用已有 Deploy Key，只创建缺失的只读授权。相同公钥若已有写权限则停止，要求管理员明确处理。
4. 按固定内部目标匹配 Webhook，创建或更新同一个 Gitea Push Hook。Secret 在读取 API 中可能被隐藏，因此重复执行会重写相同的私有 Secret，不新增 Hook。重复目标 Hook 会被拒绝，不自动删除。
5. 读取线上 assets-publish 策略，保留原语句和已有 Resource，只追加该 key 的对象前缀。确认发布账号已启用，缺少策略绑定时增量附加，不授予 consoleAdmin。
6. 更新发布白名单，生成候选 Nginx 配置，通过 nginx -t 后 reload。校验失败恢复旧 Nginx 文件，不 reload 无效配置。
7. 必要时重建接收器/Worker，确保 bind mount 使用新的配置 inode，写入私有接入记录并返回 ready。

### 重复执行与失败恢复

可以重复运行完全相同的命令。最终配置和资源不重复；已成功应用且配置未变时，`services_recreated` 为 false，不重建发布服务。命令仍核对远端状态、协调同值 Webhook Secret 和专用 IAM 策略。

绑定身份、仓库 ID、源目录、分支或 URL 前缀与已配置项不一致时停止；迁移不是 ensure 的隐式行为。分页查询覆盖已有 Deploy Key 和 Hook 列表。

多个系统之间不是事务。失败后已创建的仓库、Key、Hook 或新增 S3 权限可能保留，不会为了回滚删除用户资源。按错误阶段修复原因，再执行同一命令即可继续。即使上次停止在配置替换之后，也会根据接入记录与 inode 决定是否重建服务。

恢复材料在 `private/onboarding/<key>/`：初始 repositories 与线上策略副本、需要修改时保存的 Nginx 旧配置，以及成功后的 result.json。它们是私有材料，不提交 Git；不要未经核对直接恢复整份旧配置，以免覆盖后续其他仓库接入。

### 接入后发布

用个人 Gitea SSH 密钥克隆新仓库，按[首次发布](usage.md#首次发布空仓库)提交内容。第一次提交前不拉取不存在的远端 main 分支。若使用其他分支，将示例中的 main 替换为接入时的配置分支。

在服务器查询：

```sh
sudo ./scripts/open-assets status --key project-b
```

找到目标 SHA 的任务并确认 `published` 后，可在服务器生成链接：

```sh
sudo ./scripts/open-assets links project-b "$ASSET_COMMIT" 'public/images/logo.png'
```

先将 `ASSET_COMMIT` 设置为这次已发布的完整 SHA。图片链接的路径为 `/project-b/logo.png`，附件链接包含 `project-b/releases/<SHA>/`。命令只生成地址，不验证文件存在；生成后实际打开或下载确认。

已有提交需要补发时，执行 `sudo ./scripts/open-assets publish project-b FULL_COMMIT_SHA`，然后查询任务。它可能切换当前图片版本，且仍受任务去重限制；已清理的历史内容应使用恢复命令。

## 高级维护

以下按实际问题选择，不是第一次发布的前置步骤。恢复、核对、结案和人工确认各自保存不同证据，不需要按文档顺序全部执行。

### 对象清理账号

这不是首次发布的必要步骤。只有准备配置对象保留时，才在完成初始化后执行 `sudo ./scripts/open-assets objects setup`，创建或更新专用清理身份。重复执行保留凭据，仅为显式启用且声明独占写入的仓库授予删除权限。仓库或开关变化后需重新执行；Publisher 不增加 DeleteObject。完整步骤见[对象保留](object-retention.md)。

### 恢复已清理对象

在部署服务器执行 `sudo ./scripts/open-assets restore-objects KEY FULL_SHA`，从固定 Git 分支历史重建并核验对象。它同步执行，不依赖任务重放；获取不到 Worker 锁时返回 busy 和退出码 2。命令及前提见[对象恢复](object-restore.md)。

本地副本也已清理时，添加 `--with-local` 重建完整 public 与图片副本，默认保持当前版本。确认需要切换后，使用 `--with-local --rollback` 在完整校验后显式回滚。已有副本冲突会停止，不强制覆盖；详见[恢复与切换](object-restore.md#完整恢复后显式切换)。

### 核对恢复状态

`sudo ./scripts/open-assets check-restore main-images FULL_COMMIT_SHA` 只读核对对象、本地两份副本、current/published 与清理屏障，并保存独立诊断报告。默认不修复、不切换，也不结案原恢复。完整前提、结果与退出码见[恢复状态核对](restore-check.md)。

### 显式结案恢复

`sudo ./scripts/open-assets close-restore main-images FULL_COMMIT_SHA --reason '人工复核原因'` 在同一维护锁内重新核对，通过后单独记录结案。原恢复必须未结案或状态不确定，且 current/published 必须存在并一致；不切换版本、不解除清理屏障、不改写原报告。前提、退出码和私有审计说明见[恢复结案](restore-close.md)。

### 查询与归档恢复审计

- `restore-audit-list <key> [完整SHA] --limit 100 --offset 0`：合并查询在线与已归档的结案审计。
- `restore-audit-archive <key> [完整SHA]`：默认仅预览，保留最近 20 条、完成不足 30 天及未结案记录；添加 `--apply` 才实际归档。
- `restore-audit-verify <key> [完整SHA]`：验证归档完整性和中断清理现场，不恢复或解包到在线目录。

上面的 `<key>`、`[完整SHA]` 表示必需和可选参数，不要把尖括号或方括号原样输入。以下是只读查询与归档预览示例：

```sh
sudo ./scripts/open-assets restore-audit-list main-images --limit 20 --offset 0
sudo ./scripts/open-assets restore-audit-archive main-images
```

均在部署服务器通过 `sudo ./scripts/open-assets` 执行。归档先保存完整文件与校验清单，验证后才移出在线目录；支持中断重试，不自动删除归档包。参数、保护范围、退出码见[恢复审计](restore-audit.md)。

### 人工确认历史拒绝提醒

最新结案拒绝经核实无需处理时，使用宿主机 `acknowledge-closure` 先预览，再带本次证据令牌确认。它只确认这条提醒，不为恢复结案，不切换版本，也不解除清理屏障。查询审计 ID、填写原因和确认步骤见[人工确认说明](closure-acknowledgment.md)。
