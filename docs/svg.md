# SVG 图片发布

支持 `public/images/2026/10/10/image.svg`，初始根图片仓库的固定地址是：

```text
https://<图片域名>/2026/10/10/image.svg
```

其他仓库在该路径前添加其 mount_path。日期目录不限制文件类型；旧版本拒绝 SVG 是发布器的图片扩展名白名单所致，Gitea 与对象存储本身能保存 SVG 字节。

## 内容约束

Git 发布对所有 `.svg`（包括附件及大写扩展名）执行验证，不改写原字节：UTF-8、最多 2 MiB、20,000 个元素、128 层深度，根节点须带 SVG 命名空间。拒绝 DTD/实体声明、非 XML 声明的处理指令、非 SVG 元素、script/foreignObject 等主动元素、on* 事件属性以及外部 href/xlink:href。允许片段引用 `#id` 与内嵌 PNG/JPEG/GIF/WebP/AVIF 的 base64 图片，支持常规形状、路径、文字、内联样式和渐变。

这是一套受限静态图片协议，不是任意 SVG 应用或通用净化器。带外部字体、外链图片、HTML foreignObject 或编辑器专用 XML 节点的文件可能需要导出为普通静态 SVG。XML 校验不渲染图片，也不替代浏览器测试。

公开图片响应和 OSS 路由设置 `X-Content-Type-Options: nosniff` 与沙箱 CSP：禁用脚本、外部加载、表单及框架嵌入；允许内联样式及 data 图片。CSS 外部 url/@import 不会被当作可信资源加载。需要在正文中展示时使用 `<img src="...">`，不依赖 iframe/object 或脚本。图片入口 MIME 为 `image/svg+xml`。CSP 同时限制公开附件中的主动 HTML，可能改变原有交互附件行为。

网页 OC 上传绕过 Git 验证，不保证上传内容符合上述静态子集。公开 uploads 路由也应引用 `public-assets@file` 中间件；管理界面预览、直连 S3、自定义绕过入口的 URL 不在此响应策略保证内。仅向可信发布者授权，资源域名与管理域名分离，不共享认证 Cookie。

## 升级

1. 更新并重建 Publisher/Webhook；重新生成 Nginx 图片配置、执行 nginx 配置检查后 reload。
2. 重新运行 configure-edge.py 生成入口路由并由 file provider 加载，使公开 OSS 路由使用 public-assets 策略；保持既有域名、私有 CA 和定制配置。
3. 已配置网页公开分享的部署，在独立 uploads 路由中增加 `middlewares: [public-assets@file]`；先更新提供该中间件的主路由文件。
4. 上传后核对字节、Content-Type、nosniff 与 CSP，再检查正文 `<img>` 展示。

仅更新 Python 白名单而不更新公开响应配置，不是完整升级。旧历史版本不会自动重写；包含新验证规则不接受的 SVG 附件，重新发布或从 Git 恢复时会被拒绝，需先生成符合规则的新版本。
