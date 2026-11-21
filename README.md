# Apple 候选测试

在 Actions 中选择 **Apple 构建与运行检查 → Run workflow**。工作流使用 main 分支的配置，候选输入默认为同仓库的 apple-candidate 分支，也可以填入该分支上的完整提交 SHA。

每次运行独立检查 Xcode 和已安装的 iPhone、iPad 模拟器，记录候选 SHA，随后构建 Rust 核心、Swift 绑定和三个 Apple 目标。应用启动检查要求产生书库数据表。运行结果和模拟器截图保留七天。运行时版本见附件。

成功打包后可从该次运行的 Artifacts 下载 iOS 未签名 IPA、iOS 模拟器应用 ZIP、macOS 临时签名 ZIP 与 SHA-256 摘要。macOS 启动检查使用解压后的桌面应用。iOS 用户需自行侧载签名，macOS 产物未经 Developer ID 签名和公证。流水线不创建 Release。实体设备覆盖升级和旧钥匙串访问需要另行验收。

复验已有候选时，在同一工作流填写完整的 `candidate`、原构建的 `package_run` 和经工作流摘要核定的验收文件 `suite`（Base64）。用例在本地根目录 `test/` 维护。工作流核对原运行、候选 SHA 和包摘要，按平台与系统版本分配独立运行器。可运行全部目标、对照两个 iOS 运行时，或仅复现较新 iPhone 上的失败。各项操作结果与截图分平台保存。
