import Foundation

public struct SubscriptionUpdates: Decodable, Sendable {
  public struct Target: Decodable, Identifiable, Sendable {
    public let id: String
    public let label: String
    public let error: String
    public let source: String
    public let target: String
    public let dimension: String
    public let notify: Bool
    public let enabled: Bool
  }
  public struct Event: Decodable, Identifiable, Sendable {
    public struct Route: Decodable, Sendable {
      public let id: String
      public let workId: String
      public let chapterId: String
      public let kind: String
    }
    public let key: String
    public let title: String
    public let source: String
    public let unread: Bool
    public let createdAt: Int64
    public let route: Route
    public var id: String { key }
  }
  public let targets: [Target]
  public let events: [Event]
}

extension ProgressSession {
  public func subscriptionPostTarget(event: String, source: String) async throws -> String {
    guard let target = try await run({ store, _ in try store.subscriptionPostTarget(key: event, source: source) })
    else { throw SiteReadError.malformed }
    return target
  }

  public func setSubscriptionNotifications(_ enabled: Bool, preview: Bool) async throws {
    let name = preview ? "subscriptionNotificationPreview" : "subscriptionNotifications"
    try await run { store, _ in try store.savePreference(name: name, json: enabled ? "true" : "false") }
  }

  /// 系统授权由宿主核对。提交回执按领取批次写回，旧回调不能完成后来一次领取。
  public func deliverSubscriptionNotifications(
    systemLanguage: String,
    post: @escaping @Sendable (_ id: Int32, _ title: String, _ body: String, _ includesPreview: Bool) async throws -> Void
  ) async throws {
    struct Claim: Decodable { let key: String; let claim: Int64; let id: Int32; let title: String }
    for _ in 0..<500 {
      guard try await privacySettings().permitsSubscriptionNotification else { return }
      let claimed = try await run { store, _ in try store.claimSubscriptionNotification() }
      guard let claimed else { return }
      let event = try JSONDecoder().decode(Claim.self, from: Data(claimed.utf8))
      var delivered = false
      var failure: Error?
      do {
        let settings = try await privacySettings()
        if settings.permitsSubscriptionNotification {
          let language = settings.language.flatMap { $0.isEmpty || $0 == "system" ? nil : $0 }
          let copy = try CopyCatalog(locale: language ?? systemLanguage, pluginJson: nil)
          let preview = settings.permitsNotificationPresentation(includesPreview: true)
          let title = copy.text(key: "订阅内容有更新", params: [:])
          let body = preview ? event.title : copy.text(key: "打开应用查看更新", params: [:])
          try Task.checkCancellation()
          try await post(event.id, title, body, preview)
          delivered = true
        }
      } catch { failure = error }
      let accepted = delivered
      // 系统可能已接收通知；父任务到期时仍要完成这一次数据库回执。
      try await Task {
        try await self.run { store, _ in
          try store.finishSubscriptionNotification(key: event.key, claim: event.claim, delivered: accepted)
        }
      }.value
      if let failure { throw failure }
      if !accepted { return }
    }
  }

  public func setSubscriptionHours(_ hours: Int) async throws {
    guard [0, 1, 3, 6, 12, 24].contains(hours) else { throw SiteReadError.malformed }
    try await run { store, _ in try store.savePreference(name: "subscriptionUpdateHours", json: String(hours)) }
  }

  public func subscriptionSites(_ library: SourceLibrary, locale: String) async throws -> [SubscriptionSite] {
    let root = directory
    return try await run { _, request in
      try library.service(directory: root, request: request).subscriptionSites(locale: locale, request: request)
    }
  }

  public func saveSubscription(_ library: SourceLibrary, original: String?, source: String, dimension: String,
                               input: String, label: String) async throws -> Bool {
    let root = directory
    return try await run { store, request in
      try library.service(directory: root, request: request).saveSubscription(
        store: store, original: original, source: source, dimension: dimension, input: input, label: label, request: request)
    }
  }

  public func setSubscriptionOption(_ id: String, check: Bool? = nil, notify: Bool? = nil) async throws {
    try await run { store, _ in try store.setSubscriptionOption(id: id, check: check, notify: notify) }
  }

  public func removeSubscription(_ id: String) async throws {
    try await run { store, _ in try store.removeSubscription(id: id) }
  }

  public func subscriptionUpdates(after: SubscriptionUpdates.Event? = nil) async throws -> SubscriptionUpdates {
    try await run { store, _ in
      let targets = try JSONDecoder().decode([SubscriptionUpdates.Target].self, from: Data(store.subscriptionTargets().utf8))
      let events = try JSONDecoder().decode([SubscriptionUpdates.Event].self, from: Data(
        store.subscriptionEventPage(afterTime: after?.createdAt, afterKey: after?.key ?? "", limit: 101).utf8))
      return SubscriptionUpdates(targets: targets, events: events)
    }
  }

  public func readAllSubscriptionEvents() async throws {
    try await run { store, _ in try store.readAllSubscriptionEvents() }
  }

  public func removeSubscriptionEvents(_ key: String?) async throws {
    try await run { store, _ in try store.removeSubscriptionEvents(key: key) }
  }

  public func readSubscriptionEvent(_ id: String) async throws {
    try await run { store, _ in try store.readSubscriptionEvent(key: id) }
  }

  public func checkSubscriptions(_ library: SourceLibrary, force: Bool) async throws -> Bool {
    let settings = try await privacySettings()
    guard settings.adultConfirmed == true, force || settings.subscriptionHours > 0 else { return true }
    let root = directory
    return try await run { store, request in
      struct Result: Decodable { let failed: Int }
      let result = try library.service(directory: root, request: request)
        .checkSubscriptions(store: store, force: force, request: request)
      return try JSONDecoder().decode(Result.self, from: Data(result.utf8)).failed == 0
    }
  }
}
