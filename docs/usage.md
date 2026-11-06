# 实际使用指南

本指南对应单节点部署：代码位于 `/app/open-assets`，运行数据位于 `/data/asset-stack`。公开示例使用 `lab.dev`。请在自己的终端替换 SSH 主机别名和域名，不要修改公开文档为生产值。

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

`domains.json` 中的 `auth`、`git`、`storage`、`traefik` 是管理入口，`img`、`oss` 是公开入口。已有服务器可在本地设置 `ASSET_SSH` 后执行同样步骤，不需要重新初始化。

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

Gitea 网页受 Stargate 保护，Git SSH 不经过 HTTP 登录。SSH Push 使用你自己的密钥，不使用 Publisher 的只读 Deploy Key，也不使用服务器验收用的测试密钥。

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
export GIT_HOST=git-toh1.console.lab.dev
export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/open-assets_ed25519 -o IdentitiesOnly=yes"
git clone "ssh://git@$GIT_HOST:2222/content/images.git"
cd images
git switch main
```

默认根图片仓库为 `content/images`，发布配置的稳定 key 为 `main-images`，URL 前缀为 `/`。已有部署也可能有用于验收的 `content/project-a`，它不是日常根图片仓库。

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

把真实文件放进相应目录后提交：

```sh
mkdir -p public/images/2026/11/07 public/documents
# 将图片复制到 public/images/2026/11/07/logo.png
# 可选：将附件复制到 public/documents/manual.pdf
git pull --ff-only origin main
git add public
git diff --cached --stat
git commit -m 'Publish images and attachments'
git push origin main
git rev-parse HEAD
```

保留完整的 40 位 commit SHA。发布仅导出 `public/`；不会执行仓库脚本。图片限 PNG/JPEG/GIF/WebP/AVIF/ICO，不支持 SVG、LFS、submodule、符号链接或隐藏路径。当前实现每个发布版本必须包含 `public/images/` 中的至少一个图片文件；附件也随整个 public 目录上传。

正常发布不 force push。遇到远端更新先 pull/rebase 并解决冲突。根图片仓库不得使用已分配给其他仓库的顶层图片目录，例如 `public/images/project-a/`。

## 4. 确认发布成功

`git push` 成功仅代表 Gitea 接收了提交，不代表发布完成。在服务器执行：

```sh
cd /app/open-assets
docker compose exec -T publisher python -m open_assets status
```

找到仓库 key 与 SHA 对应的任务，只有 `published` 表示成功。`pending` / `running` 仍在处理；`failed` 需要排查；`superseded` 表示自动任务已被较新的 main 取代。相同仓库、SHA、任务类型会去重，重复 Push 不会强制重跑已失败任务。

发布故障先检查 Gitea 仓库设置 → Webhook → 最近投递，再检查任务及日志：

```sh
docker compose logs --tail=100 publisher webhook
# 只对明确失败的任务重试：
docker compose exec -T publisher python -m open_assets retry JOB_ID
```

没有任务时，确认仓库 ID、分支、enabled 和 Webhook 签名配置。首次接入或补发已存在的 commit，可显式创建人工任务：

```sh
docker compose exec -T publisher python -m open_assets publish main-images FULL_COMMIT_SHA
```

人工发布也进入队列，应继续查看 `status`。不要把修改服务器 current 链接当作正常发布方式。

## 5. 获取图片与附件链接

在自己的电脑设置真实公开主机名：

```sh
export IMG_HOST=img-toh1.lab.dev
export OSS_HOST=oss-toh1.lab.dev
export RELEASE_SHA=$(git rev-parse HEAD)
```

根仓库 `public/images/2026/11/07/logo.png` 的固定图片地址：

```text
https://img-toh1.lab.dev/2026/11/07/logo.png
```

同一图片在对象存储中的不可变版本地址：

```text
https://oss-toh1.lab.dev/attachments/repositories/main-images/releases/<完整SHA>/images/2026/11/07/logo.png
```

附件 `public/documents/manual.pdf` 的版本地址：

```text
https://oss-toh1.lab.dev/attachments/repositories/main-images/releases/<完整SHA>/documents/manual.pdf
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

返回 ready 后再提交内容；下文是对应的手工流程，用于排查或特殊部署。不要在 CLI 执行过程中同时手工修改配置。

以下以 `content/project-b`、稳定 key `project-b`、图片前缀 `/project-b/` 为例。新增必须同时完成 Git 读取权限、发布白名单、S3 写权限和 Nginx URL 映射，仅创建 Gitea 仓库不会自动发布。

### 6.1 创建仓库并授权只读拉取

在 Gitea 创建私有仓库 `content/project-b`，默认分支为 main。使用个人密钥建立初始提交，包含 `public/images/` 中至少一张有效图片。首次接入完成前尚不会自动发布。

在服务器读取 Publisher **公钥**：

```sh
cat /app/open-assets/private/publisher/deploy_key.pub
```

在新仓库的设置 → 部署密钥中添加它，保持只读。不要添加私钥，也不要授予 Publisher 写仓库权限。设置 main 分支保护并禁用 force push。

取得真实仓库 ID。在服务器管理员 shell 执行以下只读查询，它只输出 ID：

```sh
cd /app/open-assets
python3 - <<'PY'
import base64, json, pathlib, urllib.request
p = pathlib.Path('private')
auth = base64.b64encode(('content:' + (p/'admin_password').read_text().strip()).encode()).decode()
r = urllib.request.Request('http://127.0.0.1:13000/api/v1/repos/content/project-b', headers={'Authorization': 'Basic ' + auth})
print(json.load(urllib.request.urlopen(r))['id'])
PY
```

### 6.2 更新白名单

安排短暂发布维护窗口，暂停 Push；先停接收器和 Worker，避免配置变更与执行交错：

```sh
docker compose stop webhook publisher
cp -p private/repositories.yaml private/repositories.yaml.before-project-b
```

编辑 `private/repositories.yaml`，保留原项，在 repositories 下追加：

```yaml
  - key: project-b
    repository_id: 123  # 替换为刚查询的真实 ID
    name: content/project-b
    remote: ssh://git@gitea:2222/content/project-b.git
    branch: main
    enabled: true
    serve: true
    source_directory: public
    mount_path: /project-b/
    max_files: 10000
    max_bytes: 1073741824
    max_file_bytes: 104857600
```

不要把 `state_root`、`images_root` 或内部 HTTPS endpoint 改成示例值。ID、key、URL 前缀不可重复；除根路径外，前缀不嵌套。配置文件保持 Publisher 可读：

```sh
chown 20010:20010 private/repositories.yaml
chmod 0640 private/repositories.yaml
docker compose run --rm --no-deps publisher validate
```

### 6.3 扩展指定 S3 前缀权限

初始化配方把发布策略保存在 `private/oc-cli/publisher.json`。先备份，再只追加新仓库对象前缀，保留其他语句和 Resource：

```sh
cp -p private/oc-cli/publisher.json private/oc-cli/publisher.json.before-project-b
python3 - <<'PY'
import json, pathlib
p = pathlib.Path('private/oc-cli/publisher.json')
c = json.loads(p.read_text())
s = next(s for s in c['Statement'] if 's3:PutObject' in s['Action'])
resources = s['Resource'] if isinstance(s['Resource'], list) else [s['Resource']]
new = 'arn:aws:s3:::attachments/repositories/project-b/*'
if new not in resources:
    resources.append(new)
s['Resource'] = resources
p.write_text(json.dumps(c, indent=2))
PY
set -a
. ./.env
set +a
docker run --rm --network asset-stack_admin \
  -v "$PWD/private/oc-cli:/config" \
  -v "$PWD/private/tls/ca.crt:/ca.crt:ro" \
  "$OC_IMAGE" --config-dir /config admin policy add store assets-publish /config/publisher.json
```

这是已有专用发布策略的更新，不把 Publisher 改为 root 或 consoleAdmin。不需要重新设置匿名策略：默认公开策略已允许读取 attachments/repositories 下的对象。若你的部署收紧过匿名策略，应另行为新前缀增加只读授权。

### 6.4 生成并应用图片映射

先生成候选配置，再让 Nginx 校验。原服务配置保存在私有备份中：

```sh
docker compose run --rm --no-deps \
  -v "$PWD/private/generated:/generated" publisher render /generated/.candidate
cp -p private/generated/images.conf private/images.conf.before-project-b
cp private/generated/.candidate/images.conf private/generated/images.conf
if docker compose exec -T nginx nginx -c /etc/nginx/open-assets.conf -t; then
  docker compose exec -T nginx nginx -c /etc/nginx/open-assets.conf -s reload
else
  cp -p private/images.conf.before-project-b private/generated/images.conf
  echo '配置已恢复；停止操作并检查错误。'
fi
```

校验失败时不要继续。配置会把 `/project-b/` 映射到该仓库 current，未匹配的文件不会回退到根仓库。

重新创建接收器与 Worker，确保单文件 bind mount 在编辑器替换配置 inode 后也读取新配置：

```sh
docker compose up -d --no-build --force-recreate webhook publisher
```

### 6.5 配置 Hook 并首次发布

在 Gitea 新仓库设置中创建 Gitea 类型 Webhook：

- 目标：`http://webhook:9000/hooks/publish-assets`，仅内部可达。
- 内容类型：application/json；事件：Push；启用。
- Secret：与服务器 `private/webhook_secret` 文件中的值完全一致，仅在可信管理页面填写。

不要增加公网 Webhook 路由。Gitea 已只允许内部 webhook 主机，默认同一 Git 网络，无需放开全部私网地址。

之后 Push 新提交，或使用新仓库 main 的完整 SHA 手动补发：

```sh
docker compose exec -T publisher python -m open_assets publish project-b FULL_COMMIT_SHA
docker compose exec -T publisher python -m open_assets status
```

确认该 SHA 为 published 后访问：

```text
https://img-toh1.lab.dev/project-b/logo.png
https://oss-toh1.lab.dev/attachments/repositories/project-b/releases/<完整SHA>/images/logo.png
```

这里对应新仓库文件 `public/images/logo.png`。确认旧仓库仍可发布，再结束维护窗口；窗口内失败的 Hook 投递需重投或按 SHA 补发。

## 7. 停止发布与撤下访问

- `enabled: false`：拒绝新的发布及待执行任务，现有图片仍服务。修改后校验并重建 webhook/publisher。
- `serve: false`：在下一次生成、校验和 reload Nginx 后撤下该图片映射。
- 两者都不会删除已有 OSS 附件，也不会撤销其公开读取策略。需要撤回公开附件时单独处理对象或桶策略，先确认外部引用。

凭据轮换、备份和回滚见 [运行维护](operations.md)；初次部署见 [部署说明](deployment.md)。
