import ParkCore
import SwiftUI

@MainActor struct FavoriteUpdatesPage: View {
  let session: ProgressSession
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @EnvironmentObject private var downloads: DownloadQueue
  @State private var data: FavoriteUpdates?
  @State private var prefs: FavoritePreferences?
  @State private var category: Int64?
  @State private var showFilters = false
  @State private var operation: Task<Void, Never>?
  @State private var result: FavoriteCheck?
  @State private var failed = false
  @State private var entry: FavoriteUpdates.Entry?
  @State private var site: InstalledSite?

  init(session: ProgressSession, locale: String, incognito: Bool, tr: @escaping (String) -> String,
       back: @escaping () -> Void, category: Int64?) {
    self.session = session
    self.locale = locale
    self.incognito = incognito
    self.tr = tr
    self.back = back
    _category = State(initialValue: category)
  }

  var body: some View {
    Group {
      if let entry, let site, let library = InstalledSources.library {
        SiteDetails(session: session, library: library, site: site, workId: entry.id,
                    preview: nil, locale: locale, incognito: incognito, tr: tr) {
          self.entry = nil
        }
      } else {
        VStack(alignment: .leading) {
          HStack {
            Button(tr("返回"), action: back)
            Text(tr("书库更新")).font(.headline)
            Spacer()
            Button(tr("筛选")) { showFilters.toggle() }
          }.padding(.horizontal)
          List {
            if let data {
              Picker(tr("分类"), selection: $category) {
                Text(tr("全部")).tag(nil as Int64?)
                ForEach(data.categories) { group in Text(group.name).tag(Optional(group.id)) }
              }.disabled(operation != nil)
            }
            if showFilters, let prefs {
              Toggle(tr("收藏时选择分类"), isOn: Binding(get: { prefs.askCategoryOnFavorite == true }, set: { enabled in
                run { try await session.askForFavoriteCategory(enabled) }
              }))
              let hours = prefs.libraryUpdateHours ?? 0
              Picker(tr("书库更新"), selection: Binding(get: { hours }, set: { value in
                run { try await session.setFavoriteInterval(value) }
              })) {
                ForEach(Array(Set([0, 6, 12, 24, 48, 168, hours])).sorted(), id: \.self) { item in
                  Text(item == 0 ? tr("关闭") : tr("每 {hours} 小时").replacingOccurrences(of: "{hours}", with: String(item))).tag(item)
                }
              }.disabled(operation != nil)
              ForEach(FavoriteFilter.allCases, id: \.self) { filter in
                Toggle(tr(label(filter)), isOn: Binding(get: { prefs.enabled(filter) }, set: { value in
                  run { try await session.setFavoriteFilter(filter, enabled: value) }
                })).disabled(operation != nil)
              }
            }
            if operation != nil { Button(tr("取消")) { operation?.cancel() } }
            else {
              Button(tr("检查更新")) {
                run {
                  guard let library = InstalledSources.library else { throw SiteReadError.malformed }
                  result = try await session.checkFavorites(library, category: category, force: true)
                  if (result?.chapters ?? 0) > 0 { downloads.didEnqueue() }
                }
              }
            }
            if failed { Button(tr("重试")) { run {} } }
            if let result { Text(status(result)) }
            if let data {
              ForEach(data.items, id: \.self) { row in
                Button {
                  run {
                    guard let library = InstalledSources.library,
                      let installed = try await session.sites(library).first(where: { $0.key == row.source && $0.detail })
                    else { throw SiteReadError.malformed }
                    site = installed
                    entry = row
                  }
                } label: {
                  VStack(alignment: .leading) { Text(row.title); Text(row.label).font(.caption) }
                }.disabled(operation != nil)
              }
            } else if !failed { ProgressView() }
          }
        }
      }
    }.task(id: category) {
      result = nil
      failed = false
      do { try await reload() }
      catch { if !Task.isCancelled { failed = true } }
    }.onDisappear { operation?.cancel() }
  }

  private func label(_ filter: FavoriteFilter) -> String {
    switch filter {
    case .updateOnlyMultiChapter: return "只检查多章节作品"
    case .updateSkipUnread: return "跳过有未读章节的作品"
    case .updateSkipNotStarted: return "跳过还没开始读的作品"
    case .autoDownloadNew: return "自动下载新章节"
    }
  }

  private func status(_ check: FavoriteCheck) -> String {
    if check.busy == true { return tr("正在检查书库更新") }
    let failures = check.failed ?? 0
    let suffix = failures == 0 ? "" : tr("（{value0} 部检查失败）").replacingOccurrences(of: "{value0}", with: String(failures))
    let chapters = check.chapters ?? 0
    if chapters == 0 { return tr("没有新章节{value0}").replacingOccurrences(of: "{value0}", with: suffix) }
    return tr("{value0} 部作品有 {value1} 个新章节").replacingOccurrences(of: "{value0}", with: String(check.updated ?? 0))
      .replacingOccurrences(of: "{value1}", with: String(chapters)) + suffix
  }

  private func reload() async throws {
    let settings = try await session.favoritePreferences()
    let rows = try await session.favoriteUpdates(category: category)
    try Task.checkCancellation()
    prefs = settings
    data = rows
  }

  private func run(_ action: @escaping @MainActor () async throws -> Void) {
    guard operation == nil else { return }
    failed = false
    operation = Task {
      defer { operation = nil }
      do { try await action(); try await reload() }
      catch { if !Task.isCancelled { failed = true } }
    }
  }
}
