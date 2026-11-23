#if os(iOS) || os(macOS)
import CryptoKit
import Foundation
import WebKit

@MainActor public enum SiteBrowserCookies {
    private static let temporary = NSMapTable<NSString, WKWebsiteDataStore>.strongToWeakObjects()

    public static func store(for source: String) -> WKWebsiteDataStore {
        if #available(iOS 17, macOS 14, *) {
            let bytes = Array(SHA256.hash(data: Data(("park_site_" + source).utf8)).prefix(16))
            let id = UUID(uuid: (bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                                 bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]))
            return WKWebsiteDataStore(forIdentifier: id)
        }
        if let store = temporary.object(forKey: source as NSString) { return store }
        let store = WKWebsiteDataStore.nonPersistent()
        temporary.setObject(store, forKey: source as NSString)
        return store
    }

    static func cookies(_ store: WKHTTPCookieStore) async -> [HTTPCookie] {
        await withCheckedContinuation { done in store.getAllCookies { done.resume(returning: $0) } }
    }

    static func clear(_ source: String) async throws {
        let jar = store(for: source).httpCookieStore
        for cookie in await cookies(jar) {
            await withCheckedContinuation { done in jar.delete(cookie) { done.resume() } }
        }
        guard await cookies(jar).isEmpty else { throw CocoaError(.fileWriteUnknown) }
    }
}

/// 核心在后台恢复日志。每笔写入由所属站点的 WebKit 分区确认，完成后再允许核心提交。
final class SiteCookieRecovery: RecoveryBrowser, @unchecked Sendable {
    private let lock = NSLock()
    private var touched = Set<String>()

    private func onBrowser(_ work: @escaping @MainActor () async -> Bool) -> Bool {
        guard !Thread.isMainThread else { return false }
        let result = DispatchSemaphore(value: 0)
        var acknowledged = false
        Task { @MainActor in
            acknowledged = await work()
            result.signal()
        }
        result.wait()
        return acknowledged
    }

    func setCookie(source: String, url: String, header: String) -> Bool {
        guard let address = URL(string: url), !source.isEmpty,
              let parsed = HTTPCookie.cookies(withResponseHeaderFields: ["Set-Cookie": header], for: address).first
        else { return false }
        lock.lock()
        touched.insert(source)
        lock.unlock()
        let expired = parsed.expiresDate.map { $0 <= Date() } == true ||
            parsed.properties?[.maximumAge].map { (String(describing: $0) as NSString).integerValue <= 0 } == true
        return onBrowser {
            let jar = SiteBrowserCookies.store(for: source).httpCookieStore
            func same(_ other: HTTPCookie) -> Bool {
                other.name == parsed.name && other.domain.lowercased() == parsed.domain.lowercased() && other.path == parsed.path
            }
            if expired {
                for cookie in await SiteBrowserCookies.cookies(jar) where same(cookie) {
                    await withCheckedContinuation { done in jar.delete(cookie) { done.resume() } }
                }
            } else {
                await withCheckedContinuation { done in jar.setCookie(parsed) { done.resume() } }
            }
            let saved = await SiteBrowserCookies.cookies(jar).first(where: same)
            return expired ? saved == nil : saved?.value == parsed.value
        }
    }

    func flush() -> Bool {
        lock.lock()
        let sources = touched
        lock.unlock()
        return onBrowser {
            for source in sources {
                _ = await SiteBrowserCookies.cookies(SiteBrowserCookies.store(for: source).httpCookieStore)
            }
            return true
        }
    }
}
#endif
