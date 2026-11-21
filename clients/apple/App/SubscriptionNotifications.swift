#if os(iOS)
  import ParkCore
  import SwiftUI
  import UIKit
  import UserNotifications

  @MainActor final class SubscriptionNotifications: NSObject, ObservableObject, UNUserNotificationCenterDelegate {
    static let shared = SubscriptionNotifications()
    struct Open: Equatable {
      let id = UUID()
      let scene: String?
    }
    @Published private(set) var pending: Open?

    func take(scene: String?) -> Bool {
      guard let open = pending, open.scene == nil || open.scene == scene else { return false }
      pending = nil
      return true
    }

    func requestPermission() async throws -> Bool {
      let center = UNUserNotificationCenter.current()
      let settings = await center.notificationSettings()
      if settings.authorizationStatus == .notDetermined {
        return try await center.requestAuthorization(options: [.alert, .badge, .sound])
      }
      return authorized(settings)
    }

    private func authorized(_ settings: UNNotificationSettings) -> Bool {
      [.authorized, .provisional, .ephemeral].contains(settings.authorizationStatus)
    }

    func deliver(_ session: ProgressSession) async throws {
      let center = UNUserNotificationCenter.current()
      guard authorized(await center.notificationSettings()) else { return }
      try await session.deliverSubscriptionNotifications(systemLanguage: Locale.preferredLanguages.first ?? "en") {
        id, title, body, preview in
        try await self.post(id: id, title: title, body: body, preview: preview)
      }
    }

    private func post(id: Int32, title: String, body: String, preview: Bool) async throws {
      let center = UNUserNotificationCenter.current()
      guard authorized(await center.notificationSettings()) else { throw SiteReadError.malformed }
      let content = UNMutableNotificationContent()
      content.title = title
      content.body = body
      content.sound = .default
      content.userInfo = ["payload": "subscriptions", "preview": preview]
      try Task.checkCancellation()
      try await center.add(UNNotificationRequest(identifier: String(id), content: content, trigger: nil))
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
      guard response.actionIdentifier == UNNotificationDefaultActionIdentifier,
        response.notification.request.content.userInfo["payload"] as? String == "subscriptions"
      else { return }
      pending = Open(scene: response.targetScene?.session.persistentIdentifier)
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification)
      async -> UNNotificationPresentationOptions {
      let info = notification.request.content.userInfo
      guard info["payload"] as? String == "subscriptions",
        let session = try? await ProgressSession.openApplicationLibrary() else { return [] }
      let settings = try? await session.privacySettings()
      await session.close()
      // 旧通知未标明是否含作品名，按可能含预览处理。
      guard settings?.permitsNotificationPresentation(includesPreview: info["preview"] as? Bool ?? true) == true
      else { return [] }
      return [.banner, .list, .sound]
    }
  }
#endif
