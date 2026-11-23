import ParkCore
import SwiftUI

@MainActor struct SubscriptionPage: View {
  let session: ProgressSession
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @State private var data: SubscriptionUpdates?
  private struct OpenedUpdate {
    let event: SubscriptionUpdates.Event
    let site: InstalledSite
    let work: SiteWork
  }
  @State private var destination: OpenedUpdate?
  @StateObject private var prompt = SitePrompt()
  @State private var hours = 6
  #if os(iOS)
    @ObservedObject private var scheduler = SubscriptionScheduler.shared
    @State private var notifications = false
    @State private var preview = false
  #endif
  @State private var editor: SubscriptionDraft?
  @State private var removing: SubscriptionUpdates.Target?
  @State private var deletingEvent: SubscriptionUpdates.Event?
  @State private var clearingEvents = false
  @State private var pageStarts: [SubscriptionUpdates.Event] = []
  @State private var displayedPage: String?
  @State private var retry = 0
  @State private var operation: Task<Void, Never>?
  @State private var status: String?

  var body: some View {
    Group {
      if let opened = destination, let library = InstalledSources.library {
        SiteDetails(session: session, library: library, site: opened.site,
                    workId: opened.work.id,
                    preview: nil, locale: locale, incognito: incognito, tr: tr,
                    startChapter: opened.event.route.kind == "chapter" ? opened.event.route.chapterId : nil,
                    openedDetail: opened.work, openedEvent: opened.event.unread ? opened.event.key : nil) {
          destination = nil; retry += 1
        }.id(opened.event.key)
      } else {
        VStack(alignment: .leading) {
          HStack {
            Button(tr("返回"), action: back)
            Text(tr("订阅更新")).font(.headline)
            Spacer()
            if let data, !data.events.isEmpty {
              Menu(tr("管理")) {
                Button(tr("全部标为已读")) { run { try await session.readAllSubscriptionEvents() } }
                Button(tr("清空订阅更新"), role: .destructive) { clearingEvents = true }
              }
              .accessibilityLabel(tr("管理") + " · " + tr("订阅更新"))
              .disabled(operation != nil)
            }
          }.padding(.horizontal)
          if let data, !pageStarts.isEmpty || data.events.count > 100 {
            HStack {
              if !pageStarts.isEmpty {
                Button(tr("上一页")) { run(page: Array(pageStarts.dropLast())) {} }
              }
              Spacer()
              if data.events.count > 100 {
                Button(tr("下一页")) { run(page: pageStarts + [data.events[99]]) {} }
              }
            }.disabled(operation != nil).padding(.horizontal)
          }
          List {
            if let status { Text(tr(status)) }
            if let data {
              Picker(tr("自动检查订阅"), selection: Binding(get: { hours }, set: setHours)) {
                ForEach(Array(Set([0, 1, 3, 6, 12, 24, hours])).sorted(), id: \.self) { item in
                  Text(item == 0 ? tr("关闭") : tr("每 {hours} 小时").replacingOccurrences(of: "{hours}", with: String(item))).tag(item)
                }
              }.disabled(operation != nil)
              #if os(iOS)
                if scheduler.schedulingFailed { Text(tr("系统未安排后台刷新，仍可手动检查订阅。")) }
                Toggle(tr("订阅系统通知"), isOn: Binding(get: { notifications }, set: { value in
                  run {
                    if value {
                      let allowed = try await SubscriptionNotifications.shared.requestPermission()
                      guard allowed else {
                        status = "需要通知权限才能发送更新提醒"
                        return
                      }
                    }
                    try await session.setSubscriptionNotifications(value, preview: false)
                    notifications = value
                    if value { try await SubscriptionNotifications.shared.deliver(session) }
                  }
                })).disabled(operation != nil)
                if notifications {
                  Toggle(tr("通知中显示内容标题"), isOn: Binding(get: { preview }, set: { value in
                    run {
                      try await session.setSubscriptionNotifications(value, preview: true)
                      preview = value
                    }
                  })).disabled(operation != nil)
                }
              #endif
              if operation != nil {
                Button(tr("取消")) { operation?.cancel() }
              } else {
                Button(tr("检查订阅更新")) {
                  run {
                    guard let library = InstalledSources.library else { throw SiteReadError.malformed }
                    if try await !session.checkSubscriptions(library, force: true) {
                      status = "部分订阅检查失败，请查看订阅状态"
                    }
                    #if os(iOS)
                      try await SubscriptionNotifications.shared.deliver(session)
                    #endif
                  }
                }
              }
              if data.events.isEmpty { Text(tr("暂无订阅更新")) }
              ForEach(Array(data.events.prefix(100))) { event in
                HStack {
                Button {
                  run {
                    try await open(event)
                  }
                } label: {
                  VStack(alignment: .leading) {
                    Text(event.title)
                    Text(tr(event.unread ? "未读" : "已读")).font(.caption)
                  }
                }.buttonStyle(.borderless).disabled(operation != nil)
                Spacer()
                Button(role: .destructive) { deletingEvent = event } label: {
                  Image(systemName: "trash")
                }.accessibilityLabel(tr("删除订阅更新") + " " + event.title)
                 .buttonStyle(.borderless).disabled(operation != nil)
                }
              }
              Section(header: Text(tr("订阅状态"))) {
                Button(tr("添加订阅")) { editor = SubscriptionDraft(original: nil) }
                if data.targets.isEmpty { Text(tr("还没有订阅")) }
                ForEach(data.targets) { target in
                  HStack {
                    VStack(alignment: .leading) {
                      Text(target.label)
                      Text(target.error.isEmpty ? tr(target.enabled ? "检查已开启" : "检查已暂停")
                           : tr("检查失败：{code}").replacingOccurrences(of: "{code}", with: target.error)).font(.caption)
                    }
                    Spacer()
                    Menu(tr("管理")) {
                      Button(tr("编辑订阅")) { editor = SubscriptionDraft(original: target) }
                      Button(tr(target.enabled ? "暂停检查" : "恢复检查")) {
                        run { try await session.setSubscriptionOption(target.id, check: !target.enabled) }
                      }
                      Button(tr(target.notify ? "关闭此订阅通知" : "开启此订阅通知")) {
                        run { try await session.setSubscriptionOption(target.id, notify: !target.notify) }
                      }
                      Button(tr("移除订阅"), role: .destructive) { removing = target }
                    }.disabled(operation != nil)
                  }
                }
              }
            } else if status != nil {
              Button(tr("重试")) { retry += 1 }
            } else { ProgressView() }
          }.id(displayedPage)
        }
      }
    }.task(id: retry) {
      do {
        let settings = try await session.privacySettings()
        hours = settings.subscriptionHours
        #if os(iOS)
          notifications = settings.subscriptionNotifications == true
          preview = settings.subscriptionNotificationPreview == true
        #endif
        data = try await session.subscriptionUpdates(after: pageStarts.last)
      } catch { if !Task.isCancelled { status = "加载失败" } }
    }.sheet(item: $editor) { draft in
      SubscriptionEditor(session: session, original: draft.original, locale: locale, tr: tr) {
        editor = nil
        retry += 1
      }
    }.alert(tr("移除订阅"), isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
             presenting: removing) { target in
      Button(tr("取消"), role: .cancel) {}
      Button(tr("移除"), role: .destructive) { run { try await session.removeSubscription(target.id) } }
    } message: { target in Text(target.label) }
    .alert(tr("删除订阅更新"), isPresented: Binding(get: { deletingEvent != nil }, set: { if !$0 { deletingEvent=nil } }),
           presenting: deletingEvent) { event in
      Button(tr("取消"), role: .cancel) {}
      Button(tr("删除"), role: .destructive) {
        run {
          try await session.removeSubscriptionEvents(event.key)
          if data?.events.count == 1 { _ = pageStarts.popLast() }
        }
      }
    } message: { Text($0.title) }
    .alert(tr("清空订阅更新"), isPresented: $clearingEvents) {
      Button(tr("取消"), role: .cancel) {}
      Button(tr("清空"), role: .destructive) {
        run { try await session.removeSubscriptionEvents(nil); pageStarts.removeAll() }
      }
    } message: { Text(tr("仅删除更新记录，保留订阅和已检查状态，不会重复推送已发现的内容。")) }
    .siteConfirmation(prompt, tr: tr).onDisappear { operation?.cancel() }
  }

  private func open(_ event: SubscriptionUpdates.Event) async throws {
    guard let library = InstalledSources.library,
          let site = try await session.installedSites(library).first(where: { $0.key == event.source })
    else { throw SiteReadError.malformed }
    let work: SiteWork
    if event.route.kind == "post" {
      let target = try await session.subscriptionPostTarget(event: event.key, source: site.key)
      var visited = Set<String>()
      var cursor: String?
      while true {
        let page = try await prompt.perform {
          try await session.subscriptionPostPage(library, source: site.key, target: target, id: event.route.id,
                                                  cursor: cursor, confirmation: $0)
        }
        if let found = page.work { work = found; break }
        guard let next = page.next, visited.insert(next).inserted else { throw SiteReadError.malformed }
        cursor = next
      }
    } else {
      let id = event.route.kind == "chapter" ? event.route.workId : event.route.id
      guard site.detail, !id.isEmpty else { throw SiteReadError.malformed }
      work = try await prompt.perform { try await session.siteDetail(library, site: site.key, id: id, confirmation: $0) }
    }
    try Task.checkCancellation()
    destination = OpenedUpdate(event: event, site: site, work: work)
  }

  private func setHours(_ value: Int) {
    run {
      try await session.setSubscriptionHours(value)
      hours = value
      #if os(iOS)
        await scheduler.schedule(hours: value)
      #endif
    }
  }

  private func run(page: [SubscriptionUpdates.Event]? = nil, _ action: @escaping @MainActor () async throws -> Void) {
    guard operation == nil else { return }
    status = nil
    operation = Task {
      defer { operation = nil }
      do {
        try await action()
        let next = page ?? pageStarts
        let updates = try await session.subscriptionUpdates(after: next.last)
        try Task.checkCancellation()
        data = updates
        pageStarts = next
        displayedPage = next.last?.key
      } catch { if !Task.isCancelled { status = "加载失败" } }
    }
  }
}

private struct SubscriptionDraft: Identifiable {
  let original: SubscriptionUpdates.Target?
  var id: String { original?.id ?? "" }
}

@MainActor private struct SubscriptionEditor: View {
  let session: ProgressSession
  let original: SubscriptionUpdates.Target?
  let locale: String
  let tr: (String) -> String
  let saved: () -> Void
  @Environment(\.dismiss) private var dismiss
  @State private var sites: [SubscriptionSite]?
  @State private var source: String
  @State private var dimension: String
  @State private var input: String
  @State private var label: String
  @State private var busy = false
  @State private var error: String?
  @State private var attempt = 0

  init(session: ProgressSession, original: SubscriptionUpdates.Target?, locale: String, tr: @escaping (String) -> String,
       saved: @escaping () -> Void) {
    self.session = session
    self.original = original
    self.locale = locale
    self.tr = tr
    self.saved = saved
    _source = State(initialValue: original?.source ?? "")
    _dimension = State(initialValue: original?.dimension ?? "")
    _input = State(initialValue: original?.target ?? "")
    _label = State(initialValue: original?.label ?? "")
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      Text(tr(original == nil ? "添加订阅" : "编辑订阅")).font(.headline)
      Form {
        if original == nil {
          if let sites {
            if sites.isEmpty { Text(tr("不支持此订阅维度")) }
            else {
              Picker(tr("订阅站点"), selection: Binding(get: { source }, set: { value in
                source = value
                dimension = sites.first { $0.key == value }?.dimensions.first?.key ?? ""
              })) {
                ForEach(sites, id: \.key) { site in Text(site.name).tag(site.key) }
              }
              if let site = sites.first(where: { $0.key == source }) {
                Picker(tr("订阅维度"), selection: $dimension) {
                  ForEach(site.dimensions, id: \.key) { item in Text(item.label).tag(item.key) }
                }
              }
            }
          } else if error != nil { Button(tr("重试")) { attempt += 1 } }
          else { ProgressView() }
        }
        TextField(tr("订阅目标"), text: $input)
        TextField(tr("备注名称（可选）"), text: $label)
        if let error { Text(tr(error)) }
      }
      HStack {
        Button(tr("取消")) { dismiss() }
        Spacer()
        Button(tr("保存"), action: save)
          .disabled(source.isEmpty || dimension.isEmpty || input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
      }
    }
    .padding().disabled(busy).interactiveDismissDisabled(busy)
    .task(id: attempt) {
      guard original == nil else { return }
      error = nil
      do {
        guard let library = InstalledSources.library else { throw SiteReadError.malformed }
        let found = try await session.subscriptionSites(library, locale: locale)
        try Task.checkCancellation()
        sites = found
        source = found.first?.key ?? ""
        dimension = found.first?.dimensions.first?.key ?? ""
      } catch { if !Task.isCancelled { self.error = "站点加载失败，请重试" } }
    }
  }

  private func save() {
    guard !busy else { return }
    error = nil
    busy = true
    Task {
      defer { busy = false }
      do {
        guard let library = InstalledSources.library else { throw SiteReadError.malformed }
        if try await session.saveSubscription(library, original: original?.id, source: source, dimension: dimension,
                                              input: input, label: label) { saved() }
        else { error = "该订阅目标已存在" }
      } catch { self.error = "订阅保存失败，请重试" }
    }
  }
}
