# Apple 候选测试

在 Actions 中选择 **Apple 构建与运行检查 → Run workflow**。工作流使用 main 分支的配置，候选输入默认为同仓库的 apple-candidate 分支，也可以填入该分支上的完整提交 SHA。

每次运行独立检查 Xcode 和已安装的 iPhone、iPad 模拟器，记录候选 SHA，随后构建 Rust 核心、Swift 绑定和三个 Apple 目标。应用启动检查要求产生书库数据表。运行结果和模拟器截图保留七天。运行时版本见附件。

成功打包后可从该次运行的 Artifacts 下载 iOS 未签名 IPA、macOS 临时签名 ZIP 与 SHA-256 摘要。iOS 用户需自行侧载签名，macOS 产物未经 Developer ID 签名和公证。流水线不创建 Release。实体设备覆盖升级和旧钥匙串访问需要另行验收。
