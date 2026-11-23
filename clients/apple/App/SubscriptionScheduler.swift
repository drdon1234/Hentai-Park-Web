#if os(iOS)
  import BackgroundTasks
  import Combine
  import ParkCore
  import UIKit
  import UserNotifications

  @MainActor final class ParkAppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
      UNUserNotificationCenter.current().delegate = SubscriptionNotifications.shared
      BGTaskScheduler.shared.register(forTaskWithIdentifier: SubscriptionScheduler.identifier, using: .main) { task in
        guard let refresh = task as? BGAppRefreshTask else { task.setTaskCompleted(success: false); return }
        Task { @MainActor in SubscriptionScheduler.shared.begin(refresh) }
      }
      return true
    }
  }

  @MainActor final class SubscriptionScheduler: ObservableObject {
    static let shared = SubscriptionScheduler()
    static let identifier = "com.hentaipark.subscriptions"
    private let hoursKey = "parkSubscriptionHours"
    private var current: BGAppRefreshTask?
    private var worker: Task<Void, Never>?
    private var scheduleVersion = UUID()
    @Published private(set) var schedulingFailed = false

    func schedule(hours: Int) async {
      if hours == 0 { worker?.cancel() }
      await updateRequest(hours: hours)
    }

    private func updateRequest(hours: Int) async {
      let version = UUID()
      scheduleVersion = version
      let scheduler = BGTaskScheduler.shared
      schedulingFailed = false
      guard hours > 0 else {
        scheduler.cancel(taskRequestWithIdentifier: Self.identifier)
        UserDefaults.standard.set(0, forKey: hoursKey)
        return
      }
      let requests = await scheduler.pendingTaskRequests()
      guard scheduleVersion == version else { return }
      if requests.contains(where: { $0.identifier == Self.identifier }),
        UserDefaults.standard.integer(forKey: hoursKey) == hours { return }
      let request = BGAppRefreshTaskRequest(identifier: Self.identifier)
      request.earliestBeginDate = Date(timeIntervalSinceNow: Double(hours) * 3600)
      do {
        try scheduler.submit(request)
        UserDefaults.standard.set(hours, forKey: hoursKey)
      } catch { schedulingFailed = true }
    }

    func begin(_ task: BGAppRefreshTask) {
      guard current == nil else { task.setTaskCompleted(success: false); return }
      current = task
      task.expirationHandler = { [weak self, weak task] in
        Task { @MainActor in
          guard let self, let task, self.current === task else { return }
          self.worker?.cancel()
        }
      }
      worker = Task {
        var session: ProgressSession?
        var success = false
        do {
          // 冷启动时即使书库暂时受系统数据保护，也保留下次调度机会。
          let previous = UserDefaults.standard.integer(forKey: hoursKey)
          if previous > 0 { await updateRequest(hours: previous) }
          try Task.checkCancellation()
          let opened = try await ProgressSession.openApplicationLibrary()
          session = opened
          let hours = try await opened.privacySettings().subscriptionHours
          if hours != previous { await updateRequest(hours: hours) }
          try Task.checkCancellation()
          if hours > 0, let library = InstalledSources.library {
            success = try await opened.checkSubscriptions(library, force: false)
            try await SubscriptionNotifications.shared.deliver(opened)
          } else { success = hours == 0 }
        } catch { success = false }
        await session?.close()
        task.expirationHandler = nil
        current = nil
        worker = nil
        task.setTaskCompleted(success: success && !Task.isCancelled)
      }
    }
  }
#endif
