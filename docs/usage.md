# 实际使用指南

本指南对应单节点部署：代码位于 `/app/open-assets`，运行数据位于 `/data/asset-stack`。公开示例使用 `lab.dev`。请在自己的终端替换 SSH 主机别名和域名，不要修改公开文档为生产值。

日常运维先用 `sudo ./scripts/open-assets repo list` 确认 key，`doctor` 检查部署；生成正确编码的访问链接可用 `links KEY SHA PATH`。这些命令不读取管理员密码或初始化环境，详见[操作速查](operator.md)。本页涉及密码的命令只在需要登录时执行。

## 1. 查看地址和登录凭据

在自己的电脑设置部署主机的 SSH 别名：

```sh
export ASSET_SSH=your-server-alias
ssh "$ASSET_SSH"
```

以下管理命令在服务器执行。先进入管理员 shell，再进入项目目录：

```sh
sudo -i
cd /app/open-assets
cat private/domains.json
```

`domains.json` 中的 `auth`、`git`、`storage`、`traefik`、`info` 是管理入口，`img`、`oss` 是公开入口。已有服务器可在本地设置 `ASSET_SSH` 后执行同样步骤，不需要重新初始化。

管理页面有两层认证：先使用 Stargate 密码，再使用应用自身账号。

- Stargate：只输入 `private/stargate_password` 中的密码，没有额外用户名。
- Gitea：初始化账号为 `content`，密码在 `private/admin_password`。
- OC：Access Key 在 `private/console_user`，Secret Key 在 `private/console_password`。默认 Access Key 为 `assets-operator`，以实际文件为准。
- Traefik：通过 Stargate 后直接进入 Dashboard，无第二个应用账号。

仅在自己的可信终端按需查看对应文件：

```sh
cat private/stargate_password
# 登录 Gitea 时才查看：
cat private/admin_password
# 登录 OC 时才查看：
cat private/console_user
cat private/console_password
```

不要将输出贴到工单、聊天、截图、Git 提交或公开日志。`private/storage/` 是存储 root 凭据，`private/publisher/` 是发布程序凭据；日常使用无需读取或复制它们。不要重复运行初始化脚本来“查看密码”，它会修改运行状态。

OC 的“退出登录”退出 OC 自身会话，不等于退出 Stargate 或 Gitea；刷新后应回到 OC IAM 登录页。Stargate 重启后需要重新认证。

## 2. 配置个人 Git SSH 密钥

Gitea 网页受 Stargate 保护，Git SSH 不经过 HTTP 登录。SSH Push 使用你自己的密钥，不使用 Publisher 的只读 Deploy Key。

在自己的电脑查看现有公钥，或创建一把独立密钥：

```sh
# 已有专用密钥时跳过生成；不要覆盖原文件。
test ! -e "$HOME/.ssh/open-assets_ed25519" && \
  test ! -e "$HOME/.ssh/open-assets_ed25519.pub" && \
  ssh-keygen -t ed25519 -f "$HOME/.ssh/open-assets_ed25519" -C open-assets-user
cat "$HOME/.ssh/open-assets_ed25519.pub"
```

登录 Gitea → 个人设置 → SSH / GPG 密钥 → 添加 SSH 公钥。只添加 `.pub` 的内容，保留私钥在自己的电脑。多人使用时，管理员应创建各自的 Gitea 账号并授予所需仓库权限。

首次连接先核对 SSH 主机指纹。在服务器可信 SSH 会话中查看：

```sh
ssh-keygen -lf /data/asset-stack/gitea/data/ssh/gitea.ed25519.pub
```

与本地 SSH 提示的对应算法指纹一致后再接受，不关闭主机密钥检查。

## 3. 克隆图片仓库并发布

在自己的电脑执行，将 `GIT_HOST` 替换为实际 `git` 主机名：

```sh
export GIT_HOST=git-node-name.console.lab.dev
export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/open-assets_ed25519 -o IdentitiesOnly=yes"
git clone "ssh://git@$GIT_HOST:2222/content/images.git"
cd images
```

默认根图片仓库为 `content/images`，发布配置的稳定 key 为 `main-images`，URL 前缀为 `/`。

保持以下结构：

```text
public/
├── images/
│   └── 2026/11/07/logo.png
├── documents/
│   └── manual.pdf
└── packages/
    └── application.tar.gz
```

### 首次发布（空仓库）

初始化和新增仓库都不会创建内容提交。第一次提交前不要执行 `git pull origin main`，因为远端还没有这个分支。先放好真实图片，再提交：

```sh
# 首次发布（空仓库）
test "$(git branch --show-current)" = main || git switch -c main
mkdir -p public/images/2026/11/07 public/documents
# 将图片复制到 public/images/2026/11/07/logo.png
# 可选：将附件复制到 public/documents/manual.pdf
git add public
git diff --cached --stat
git commit -m 'Publish images and attachments'
git push -u origin main
git rev-parse HEAD
```

如果 Git 提示缺少提交身份，先在此仓库设置自己的 `user.name` 和 `user.email`，再执行 commit。它们与 Gitea 登录账号、SSH 密钥是不同配置。

### 后续更新

先同步远端，再复制新文件或修改现有文件：

```sh
# 后续更新
git switch main
git pull --ff-only origin main
# 修改 public/ 中的文件
git add public
git diff --cached --stat
git commit -m 'Update assets'
git push origin main
git rev-parse HEAD
```

保留完整的 40 位 commit SHA。发布仅导出 `public/`；不会执行仓库脚本。图片限 PNG/JPEG/GIF/WebP/AVIF/ICO，不支持 SVG、LFS、submodule、符号链接或隐藏路径。当前实现每个发布版本必须包含 `public/images/` 中的至少一个图片文件；附件也随整个 public 目录上传。

正常发布不 force push。遇到远端更新先 pull/rebase 并解决冲突。根图片仓库不得使用已分配给其他仓库的顶层图片目录，例如 `public/images/project-a/`。

## 4. 确认发布成功

`git push` 成功仅代表 Gitea 接收了提交，不代表发布完成。在服务器执行：

```sh
cd /app/open-assets
sudo ./scripts/open-assets status
```

找到仓库 key 与 SHA 对应的任务，只有 `published` 表示成功。`pending` / `running` 仍在处理；`failed` 需要排查；`superseded` 表示自动任务已被较新的 main 取代。相同仓库、SHA、任务类型会去重，重复 Push 不会强制重跑已失败任务。

发布故障先检查 Gitea 仓库设置 → Webhook → 最近投递，再检查任务及日志：

```sh
docker compose logs --tail=100 publisher webhook
# 只对明确失败的任务重试：
sudo ./scripts/open-assets retry JOB_ID
```

没有任务时，确认仓库 ID、分支、enabled 和 Webhook 签名配置。首次接入或补发已存在的 commit，可显式创建人工任务：

```sh
sudo ./scripts/open-assets publish main-images FULL_COMMIT_SHA
```

人工发布也进入队列，应继续查看 `status`。不要把修改服务器 current 链接当作正常发布方式。

## 5. 获取图片与附件链接

在自己的电脑设置真实公开主机名：

```sh
export IMG_HOST=img-node-name.lab.dev
export OSS_HOST=oss-node-name.lab.dev
export RELEASE_SHA=$(git rev-parse HEAD)
```

根仓库 `public/images/2026/11/07/logo.png` 的固定图片地址：

```text
https://img-node-name.lab.dev/2026/11/07/logo.png
```

同一图片在对象存储中的不可变版本地址：

```text
https://oss-node-name.lab.dev/attachments/repositories/main-images/releases/<完整SHA>/images/2026/11/07/logo.png
```

附件 `public/documents/manual.pdf` 的版本地址：

```text
https://oss-node-name.lab.dev/attachments/repositories/main-images/releases/<完整SHA>/documents/manual.pdf
```

也可在本地打印链接：

```sh
printf 'https://%s/2026/11/07/logo.png\n' "$IMG_HOST"
printf 'https://%s/attachments/repositories/main-images/releases/%s/documents/manual.pdf\n' "$OSS_HOST" "$RELEASE_SHA"
```

图片路径去掉 `public/images/`，并添加仓库的 `mount_path`；附件路径去掉 `public/`，添加桶、稳定 key 和 SHA。两者的路径规则不同。

中文及特殊字符按路径段进行 URL 编码；空格用 `%20`、加号用 `%2B`、百分号用 `%25`，保留目录分隔 `/`。可以用 Python 生成完整链接：

```sh
python3 - <<'PY'
import os
from urllib.parse import quote
key = 'documents/中文 文件+%.pdf'
print(f'https://{os.environ["OSS_HOST"]}/attachments/repositories/main-images/releases/{os.environ["RELEASE_SHA"]}/' + quote(key, safe='/'))
PY
```

固定图片 URL 随最新成功发布变化，默认缓存 300 秒；附件 SHA URL 长期不变，适合文章、版本发布及回溯。发布前不要对外引用尚未校验的 SHA 路径。图片回滚不会删除旧附件。OC 中“下载/复制文件链接”得到的是受会话保护的 OC 下载入口，不是公开 OSS 链接；对外分享应按上面的公开路径生成。

## 6. 新增白名单仓库

推荐使用 [仓库自动接入 CLI](cli.md)：在服务器运行以下命令即可协调仓库、只读 Deploy Key、Webhook、S3 前缀权限和 Nginx 映射，支持重复执行。

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo ensure content/project-b --key project-b --mount-path /project-b/
```

返回 `status: ready` 后，在自己的电脑克隆新仓库，按[首次发布](#首次发布空仓库)提交图片。已有仓库可重复执行相同命令，工具会复用仓库、Key 和 Hook。接入失败时先按输出阶段排查，再重试同一命令。

具体权限和参数见[仓库 CLI](cli.md)；特殊部署的[手工接入步骤](manual-onboarding.md)单独列出。

## 7. 停止发布与撤下访问

- `enabled: false`：拒绝新的发布及待执行任务，现有图片仍服务。修改后校验并重建 webhook/publisher。
- `serve: false`：在下一次生成、校验和 reload Nginx 后撤下该图片映射。
- 两者都不会删除已有 OSS 附件，也不会撤销其公开读取策略。需要撤回公开附件时单独处理对象或桶策略，先确认外部引用。

凭据轮换、备份和回滚见 [运行维护](operations.md)；初次部署见 [部署说明](deployment.md)。
