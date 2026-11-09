# 分享第一个文件

先确认[部署](deployment.md)或[新服务器路线](new-server.md)已经完成，管理 HTTPS 入口可登录。第一次分享只需选择下面一条路线，备份、保留和高级恢复可随后配置。

- **Git 发布**：默认入口已配置好，适合固定图片地址和可追溯的版本附件。
- **网页上传**：不需要个人 Git 密钥，但管理员需先按[网页公开目录](web-uploads.md)配置独立桶和只读路由；上传成功本身不等于已经公开。

## 路线一：通过 Git 发布

### 1. 添加个人公钥并克隆

按[使用指南](usage.md#1-查看地址和登录凭据)取得访问地址，在 Gitea 添加自己的 SSH 公钥，核对服务器指纹；完整步骤见[SSH 密钥配置](usage.md#2-配置个人-git-ssh-密钥)。不要使用 Publisher 的只读 Deploy Key。

在自己的电脑打开终端，替换主机名后执行。下面的密钥路径对应使用指南生成的密钥：

```sh
export GIT_HOST=git-node-name.console.lab.dev
export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/open-assets_ed25519 -o IdentitiesOnly=yes"
git clone "ssh://git@$GIT_HOST:2222/content/images.git"
cd images
```

本例用于空的初始仓库。如果已有内容或其他人发布过，按[后续更新](usage.md#后续更新)先同步，不能把空仓库步骤用于已有历史。

### 2. 创建图片与附件并 Push

下面生成一张 1×1 PNG 和一份文本文件，只用于验证。文件已存在时停止，不覆盖原内容：

```sh
(
  set -eu
  test ! -e public/images/hello.png && test ! -L public/images/hello.png
  test ! -e public/documents/hello.txt && test ! -L public/documents/hello.txt
  mkdir -p public/images public/documents
  set -C
  printf '%s' 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII=' | openssl base64 -d -A > public/images/hello.png
  printf 'Hello Open Assets\n' > public/documents/hello.txt
)
```

若 Git 尚未配置提交身份，先用自己的姓名和邮箱设置此仓库的 `user.name`、`user.email`，再提交。这与 Gitea 密码和 SSH 公钥不同。

```sh
test "$(git branch --show-current)" = main || git switch -c main
git add public
git diff --cached --stat
git commit -m 'Publish first files'
git push -u origin main
git rev-parse HEAD
```

检查暂存清单中只有准备公开的文件。保留最后输出的完整 40 位 SHA；Push 成功只说明 Gitea 收到了提交。

### 3. 确认发布并取得链接

由管理员在服务器执行：

```sh
cd /app/open-assets
sudo ./scripts/open-assets status --key main-images
```

找到与刚才 SHA 对应的任务，等到 published。如果没有任务或 failed，按[排查步骤](troubleshooting.md#push-成功但没有任务)处理，不要重跑初始化。

在服务器终端将 `ASSET_COMMIT` 设置为这次已发布的完整 SHA，再生成地址：

```sh
sudo ./scripts/open-assets links main-images "$ASSET_COMMIT" public/images/hello.png
sudo ./scripts/open-assets links main-images "$ASSET_COMMIT" public/documents/hello.txt
```

图片有固定 URL，也有版本附件 URL；文本文件只使用版本附件 URL。links 不验证对象存在，下一步需要实际下载。

### 4. 从匿名客户端下载验证

在自己的电脑、刚才的内容仓库目录内，替换真实公开主机名后执行：

```sh
export IMG_HOST=img-node-name.lab.dev
export OSS_HOST=oss-node-name.lab.dev
export RELEASE_SHA=$(git rev-parse HEAD)
DOWNLOAD_DIR=$(mktemp -d)
curl --fail "https://$IMG_HOST/hello.png" -o "$DOWNLOAD_DIR/hello.png"
curl --fail "https://$OSS_HOST/attachments/repositories/main-images/releases/$RELEASE_SHA/documents/hello.txt" -o "$DOWNLOAD_DIR/hello.txt"
cmp public/images/hello.png "$DOWNLOAD_DIR/hello.png"
cmp public/documents/hello.txt "$DOWNLOAD_DIR/hello.txt"
```

这里的 HEAD 必须仍是刚才的发布提交；如果之后又提交过，改填已发布 SHA。curl 不带管理 Cookie，成功下载且 cmp 无输出，表示本例公开读取与字节校验通过。也可在无痕窗口打开这两个链接。

图片固定 URL 会随之后的成功发布变化，默认缓存 300 秒；附件 URL 指向具体版本，只在对象保留期间有效。没有目录列表，打开域名首页 404 不代表具体文件有问题。

## 路线二：通过网页上传

管理员先完整执行[网页上传并公开分享](web-uploads.md)：创建专用桶、只读匿名策略和精确的公开路由，验证后交付使用者地址及受限凭据。之后：

1. 登录 OC，在 `uploads/public/` 上传 `hello.txt`。
2. 从 OSS 域名访问 `/uploads/public/hello.txt`，不复制 OC 的会话下载地址对外分享。
3. 用无痕窗口或不带 Cookie 的 curl 下载，并比对本地原文件。

这条路线不创建 Git 提交、Publisher 任务或 Nginx 图片版本。不要等 published，也不要在 Git 的版本目录上传；需要自动版本记录时改用 Git 路线。

## 下一步

- 日常更新和链接编码：[使用指南](usage.md)。
- 为其他项目配置仓库：[CLI](cli.md)。
- 查看健康和磁盘、启用日志轮转、安排备份：[Info](info.md)、[日志](logging.md)、[维护](operations.md)。
- 需要时再配置[本地保留](retention.md)、[对象保留](object-retention.md)和[恢复](object-restore.md)。第一次分享不要求开启删除或执行恢复结案。
