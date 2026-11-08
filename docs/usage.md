# 使用指南

部署完成后，可以通过网页管理存储文件，也可以通过 Git 自动发布图片与附件。

- 只想在网页上传、下载文件：先[查看登录地址和凭据](#1-查看地址和登录凭据)，再看[网页管理文件](#通过网页管理文件)。
- 想用 Git 发布：依次完成[SSH 密钥配置](#2-配置个人-git-ssh-密钥)、[首次提交](#3-克隆图片仓库并发布)、[确认发布](#4-确认发布成功)和[获取链接](#5-获取图片与附件链接)。
- 为另一个项目配置仓库：看[仓库接入](#6-新增白名单仓库)。
- 想先完成一个可验证的小例子：看[首次分享文件](first-share.md)。
- 还没有部署服务：先看[环境要求](requirements.md)，选择[全新服务器](new-server.md)或[已有入口部署](deployment.md)。

下面按默认目录说明：服务器代码在 `/app/open-assets`，数据在 `/data/asset-stack`。示例域名 `lab.dev` 和 SSH 别名都需替换为自己的值；真实配置只保存在私有环境中。

命令会注明执行位置。“自己的电脑”指存放个人 Git 密钥和内容仓库的电脑；“服务器”指运行 Open Assets 的 Linux 主机。已进入服务器 root shell 时，可以省略命令中的 `sudo`。

## 1. 查看地址和登录凭据

在自己的电脑执行。`ASSET_SSH` 使用你已在 SSH 配置中设置的主机别名；也可以填入 `用户名@服务器地址`。这条命令只选择连接目标，不会创建 SSH 配置：

```sh
export ASSET_SSH=your-server-alias
ssh "$ASSET_SSH"
```

以下凭据查询由服务器管理员执行。普通使用者应向管理员获取访问地址和自己的账号，不需要服务器 root 权限；不要共用管理员账号。管理员先进入 root shell，再进入项目目录：

```sh
sudo -i
cd /app/open-assets
cat private/domains.json
```

`domains.json` 中的 `auth`、`git`、`storage`、`traefik`、`info` 是管理入口，`img`、`oss` 是公开入口。已有服务器可在本地设置 `ASSET_SSH` 后执行同样步骤，不需要重新初始化。

用浏览器打开对应的 `https://` 地址。管理页面先经过 Stargate；Gitea 和 OC 还需要登录各自账号，Traefik 与 Info 没有第二层应用登录。

- Stargate：只输入 `private/stargate_password` 中的密码，没有额外用户名。
- Gitea：初始化账号为 `content`，密码在 `private/admin_password`。
- OC：Access Key 在 `private/console_user`，Secret Key 在 `private/console_password`。默认 Access Key 为 `assets-operator`，以实际文件为准。
- Traefik / Info：通过 Stargate 后进入 Dashboard 或运行看板，无第二个应用账号。

仅在自己的可信终端按需查看对应文件：

```sh
cat private/stargate_password
# 登录 Gitea 时才查看：
cat private/admin_password
# 登录 OC 时才查看：
cat private/console_user
cat private/console_password
```

不要将输出贴到工单、聊天、截图、Git 提交或公开日志。`private/storage/` 是存储 root 凭据，`private/publisher/` 是发布程序凭据；日常使用无需读取或复制它们。初始化脚本不是凭据查询工具：已有配置或初始化记录会触发保护并拒绝执行。查找密码应读取上面的私有文件，不要删除保护记录后重新初始化。

Stargate 重启后需要重新认证。OC 的退出方式见下面的网页操作说明。

## 通过网页管理文件

网页上传用于直接管理对象存储，不需要配置个人 Git 密钥：

1. 打开 `domains.json` 中 `storage` 对应的 HTTPS 地址，先通过 Stargate，再使用 OC 的 Access Key 和 Secret Key 登录。
2. 在 OC 中打开有权限访问的桶和目录，选择上传文件。上传后核对对象名称、大小，并下载一份确认内容。
3. 离开时使用 OC 的“退出登录”。这只退出 OC，不会同时退出 Stargate 或 Gitea。

网页上传不会把文件写入 Git，也不会更新 Nginx 的固定图片地址。默认公开入口只允许读取 `attachments` 桶下 `repositories/` 前缀的对象；其他位置不会因为上传成功就自动公开。OC 的下载或复制链接通常是管理入口，不能直接当作公开分享地址。

Git 管理的 `repositories/<key>/releases/<SHA>/` 由发布程序维护，不要为获得公开链接而直接向其中上传或修改文件，否则会与校验清单不一致。独立的网页上传分享目录需要管理员另外配置存储权限和公开路由，默认配方没有自动创建这类目录。按[网页上传并公开分享](web-uploads.md)创建独立的 `uploads/public/`，验证后再用于日常上传。

需要固定图片地址或按 Git 版本发布时，继续下面的步骤。

## 2. 配置个人 Git SSH 密钥

Gitea 网页受 Stargate 保护，Git SSH 不经过 HTTP 登录。SSH Push 使用你自己的密钥，不使用 Publisher 的只读 Deploy Key。

在自己的电脑另开一个终端，查看现有公钥，或创建一把独立密钥。下面的 `$HOME` 应是你的本地用户目录，不是在服务器 root shell 中执行：

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

只需要准备 `public/` 中的内容，其他仓库文件不会发布。图片放在 `public/images/`，附件可放在 `public/documents/` 等目录：

```text
public/
├── images/
│   └── 2026/11/07/logo.png
├── documents/
│   └── manual.pdf
└── packages/
    └── application.tar.gz
```

每个 Git 发布版本至少需要一张图片。图片支持 PNG/JPEG/GIF/WebP/AVIF/ICO；SVG、LFS、submodule、符号链接和隐藏路径不支持。附件会随整个 `public/` 目录上传；仓库脚本不会执行。

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

最后一条命令输出完整的 40 位提交编号（commit SHA）。保留这次准备发布的 SHA，后面用它查询任务和生成版本链接。

日常更新不要使用 force push。`git pull --ff-only` 如果提示分支已分叉，先检查双方提交并解决冲突，再重新提交。根图片仓库不得使用已分配给其他仓库的顶层图片目录，例如 `public/images/project-a/`。

## 4. 确认发布成功

`git push` 成功仅代表 Gitea 接收了提交，不代表发布完成。在服务器执行：

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo list
sudo ./scripts/open-assets status --key main-images
```

`repo list` 列出配置中的仓库。key 是 Open Assets 使用的稳定标识，初始 `content/images` 仓库对应 `main-images`；查询其他仓库时替换为它的 key。找到与刚才 SHA 对应的任务，只有 `published` 表示成功。`pending` / `running` 仍在处理；`failed` 需要排查；`superseded` 表示自动任务已被较新的 main 取代。相同仓库、SHA、任务类型会去重，重复 Push 不会强制重跑已失败任务。

发布故障先检查 Gitea 仓库设置 → Webhook → 最近投递，再在服务器查看日志。以普通运维账号执行 Docker 命令时需要 `sudo`：

```sh
sudo docker compose logs --tail=100 publisher webhook
# 只对明确失败的任务重试：
sudo ./scripts/open-assets retry JOB_ID
```

没有任务时，确认仓库 ID、分支、enabled 和 Webhook 签名配置。首次接入或补发已存在的 commit，可显式创建人工任务：

```sh
sudo ./scripts/open-assets publish main-images FULL_COMMIT_SHA
```

`JOB_ID` 替换为 status 输出中失败任务的完整 ID，`FULL_COMMIT_SHA` 替换为目标提交的完整 SHA。人工发布可能切换当前图片版本，执行后仍要查询 `status`；不要手工修改服务器的 `current` 链接。更多排查见[故障排查](troubleshooting.md)，日常命令见[操作速查](operator.md)。

## 5. 获取图片与附件链接

确认目标 SHA 已发布后再生成链接。网页上传的文件不适用下面的 Git 版本映射。

在自己的电脑、内容仓库目录内执行，设置真实公开主机名。下面从 HEAD 获取 SHA，只适用于刚刚发布的就是当前提交；引用历史版本时应填入那个版本的完整 SHA：

```sh
export IMG_HOST=img-node-name.lab.dev
export OSS_HOST=oss-node-name.lab.dev
export RELEASE_SHA=$(git rev-parse HEAD)
```

也可以在服务器用链接命令自动处理路径编码：

```sh
cd /app/open-assets
sudo ./scripts/open-assets links main-images "$ASSET_COMMIT" \
  'public/images/2026/11/07/logo.png'
```

先在服务器终端将 `ASSET_COMMIT` 设置为已发布的完整 SHA。命令只生成地址，不检查文件是否存在；复制后应实际打开或下载确认。

下面以初始根图片仓库为例，说明两种地址的区别。`public/images/2026/11/07/logo.png` 的固定图片地址：

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

固定图片 URL 随最新成功发布变化，默认缓存 300 秒；附件 SHA URL 的路径和内容保持不变，适合文章、版本发布及回溯，但只在对象仍被保留时有效。实际清理对象后，旧链接会失效；需要长期引用的版本应纳入[固定保留策略](object-retention.md)。发布前不要对外引用尚未校验的 SHA 路径。图片回滚不会删除旧附件。OC 中“下载/复制文件链接”得到的是受会话保护的 OC 下载入口，不是公开 OSS 链接；对外分享应按上面的公开路径生成。

## 6. 新增白名单仓库

为另一个项目分配独立图片路径时，在服务器使用[仓库自动接入 CLI](cli.md)。它会协调仓库、只读 Deploy Key、Webhook、S3 前缀权限和 Nginx 映射，支持重复执行。接入期间可能短暂停止或重建发布服务，请暂停 Push，等待 `ready` 后再提交；已有图片和附件继续提供访问。

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo ensure content/project-b --key project-b --mount-path /project-b/
```

这里 `content/project-b` 是 Gitea 仓库名，`project-b` 是稳定 key，`/project-b/` 是图片访问前缀。发布 `public/images/logo.png` 后，图片路径为 `/project-b/logo.png`，附件路径中的仓库 key 也改为 `project-b`。

返回 `status: ready` 后，在自己的电脑克隆 `content/project-b`（将前面 clone 命令中的 `content/images.git` 改为 `content/project-b.git`），按[首次发布](#首次发布空仓库)提交图片。已有仓库可重复执行相同命令，工具会复用仓库、Key 和 Hook。接入失败时先按输出阶段排查，再重试同一命令。

具体权限和参数见[仓库 CLI](cli.md)；特殊部署的[手工接入步骤](manual-onboarding.md)单独列出。

## 7. 停止发布与撤下访问

- `enabled: false`：拒绝新的发布及待执行任务，现有图片仍服务。修改后校验并重建 webhook/publisher。
- `serve: false`：在下一次生成、校验和 reload Nginx 后撤下该图片映射。
- 两者都不会删除已有 OSS 附件，也不会撤销其公开读取策略。需要撤回公开附件时单独处理对象或桶策略，先确认外部引用。

凭据轮换、备份和回滚见 [运行维护](operations.md)；初次部署见 [部署说明](deployment.md)。
