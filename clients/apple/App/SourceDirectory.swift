import ParkCore
import SwiftUI

extension Notification.Name {
  static let parkSiteOrderChanged = Notification.Name("ParkSiteOrderChanged")
}

@MainActor final class SiteDisplayOrder: ObservableObject {
  @Published private(set) var elapsed: [String: TimeInterval] = [:]
  @Published private(set) var measured = false

  func ordered(_ sites: [InstalledSite]) -> [InstalledSite] {
    guard measured else { return sites }
    return sites.enumerated().sorted { first, second in
      let a = elapsed[first.element.id] ?? .infinity, b = elapsed[second.element.id] ?? .infinity
      return a == b ? first.offset < second.offset : a < b
    }.map(\.element)
  }

  func clear() { elapsed = [:]; measured = false }

  func measure(_ session: ProgressSession, library: SourceLibrary) async throws {
    let sites = try await session.sites(library).filter { $0.unavailable != true && $0.entryFeed != nil }
    var result: [String: TimeInterval] = [:]
    await withTaskGroup(of: (String, TimeInterval?).self) { group in
      var remaining = sites.makeIterator()
      func submit(_ site: InstalledSite) {
        group.addTask {
          let start = ProcessInfo.processInfo.systemUptime
          let elapsed = await withTaskGroup(of: TimeInterval?.self) { probe in
            probe.addTask {
              do {
                _ = try await session.sitePage(library, site: site, keyword: "", page: 1, cursor: nil)
                return ProcessInfo.processInfo.systemUptime - start
              } catch { return nil }
            }
            probe.addTask { try? await Task.sleep(nanoseconds: 8_000_000_000); return nil }
            let first = await probe.next() ?? nil
            probe.cancelAll()
            return first
          }
          return (site.id, elapsed)
        }
      }
      for _ in 0..<6 { if let site = remaining.next() { submit(site) } }
      while let (id, time) = await group.next() {
        if let time { result[id] = time }
        if !Task.isCancelled, let site = remaining.next() { submit(site) }
      }
    }
    try Task.checkCancellation()
    elapsed = result
    measured = true
  }
}

enum InstalledSources {
  static let library: SourceLibrary? = {
    guard let resources = Bundle.main.url(forResource: "ParkSources", withExtension: nil),
      let cache = try? FileManager.default.url(
        for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
    else { return nil }
    return SourceLibrary(resources: resources, cache: cache)
  }()
}

@MainActor struct SourceDirectory: View {
  @EnvironmentObject private var displayOrder: SiteDisplayOrder
  let session: ProgressSession
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let close: () -> Void
  @State private var sites: [InstalledSite]?
  @State private var chosen: InstalledSite?
  @State private var managing = false
  @State private var failed = false
  @State private var retry = 0

  var body: some View {
    if managing, let library = InstalledSources.library {
      SiteOrderView(session: session, library: library, tr: tr) {
        managing = false
        retry += 1
      }
    } else if let chosen, let library = InstalledSources.library {
      SourceResults(
        session: session, library: library, site: chosen, locale: locale, incognito: incognito,
        tr: tr
      ) { self.chosen = nil }.id(chosen.id)
    } else {
      VStack(alignment: .leading) {
        HStack {
          Button(tr("返回"), action: close)
          Text(tr("全部站点")).font(.headline)
          Spacer()
          Button(tr("站点管理")) { managing = true }
        }.padding()
        if failed {
          Button(tr("站点加载失败，请重试")) { retry += 1 }.padding()
        } else if let sites {
          if sites.isEmpty { Text(tr("暂无内容")).padding() }
          List(displayOrder.ordered(sites)) { site in
            Button {
              chosen = site
            } label: {
              VStack(alignment: .leading) {
                Text(site.name ?? site.key)
                if site.unavailable == true { Text(tr("站点加载失败，请重试")).font(.caption) }
              }
            }
          }
        } else {
          ProgressView().frame(maxWidth: .infinity)
        }
      }.task(id: retry) {
        failed = false
        do {
          guard let library = InstalledSources.library else { throw CocoaError(.fileNoSuchFile) }
          sites = try await session.sites(library)
        } catch { if !Task.isCancelled { failed = true } }
      }
    }
  }
}

@MainActor struct SiteOrderView: View {
  @EnvironmentObject private var displayOrder: SiteDisplayOrder
  let session: ProgressSession
  let library: SourceLibrary
  let tr: (String) -> String
  let back: () -> Void
  @State private var rows: [ProgressSession.ManagedSite] = []
  @State private var busy = false
  @State private var failed = false
  @State private var confirm = false
  @State private var categories = false
  @State private var measuring = false

  var body: some View {
    VStack(alignment: .leading) {
      HStack {
        Button(tr("返回"), action: back)
        Text(tr("站点管理")).font(.headline)
        Spacer()
        Button(tr("恢复站点默认顺序")) { confirm = true }.disabled(measuring)
      }.padding()
      Button(tr("分类顺序")) { categories = true }
      if measuring {
        Button(tr("取消探测")) { measuring = false }
        ProgressView()
      } else {
        Button(tr("测速并自动排序")) { measuring = true }.disabled(rows.isEmpty)
        if displayOrder.measured { Button(tr("恢复原显示顺序")) { displayOrder.clear() } }
      }
      if failed { Button(tr("重试")) { Task { await update {} } }.padding(.horizontal) }
      if busy { ProgressView() }
      List(Array(rows.enumerated()), id: \.element.id) { index, row in
        HStack {
          Text(row.name)
          if let elapsed = displayOrder.elapsed[row.id] {
            Text("\(Int((elapsed * 1000).rounded())) ms").font(.caption.monospacedDigit())
          }
          if !row.enabled { Text(tr("未启用")).font(.caption) }
          Spacer()
          Button { move(index, -1) } label: { Image(systemName: "arrow.up") }
            .accessibilityLabel(Text(tr("上移") + " " + row.name)).disabled(index == 0)
          Button { move(index, 1) } label: { Image(systemName: "arrow.down") }
            .accessibilityLabel(Text(tr("下移") + " " + row.name)).disabled(index + 1 == rows.count)
        }.buttonStyle(.borderless).disabled(measuring)
      }
    }.disabled(busy)
    .sheet(isPresented: $categories) {
      CategoryOrderView(session: session, tr: tr) { categories = false }
    }
    .task { await update {} }
    .task(id: measuring) {
      guard measuring else { return }
      do { try await displayOrder.measure(session, library: library) }
      catch { if !Task.isCancelled { failed = true } }
      if !Task.isCancelled { measuring = false }
    }
    .alert(tr("恢复站点默认顺序"), isPresented: $confirm) {
      Button(tr("取消"), role: .cancel) {}
      Button(tr("恢复")) { Task { await update {
        try await session.restoreSiteOrder(library)
        displayOrder.clear()
        NotificationCenter.default.post(name: .parkSiteOrderChanged, object: nil)
      } } }
    } message: { Text(tr("将替换当前手动排列的站点顺序，是否继续？")) }
  }

  private func move(_ index: Int, _ offset: Int) {
    guard rows.indices.contains(index + offset) else { return }
    let key = rows[index].id, anchor = rows[index + offset].id
    Task { await update {
      try await session.moveSite(library, key: key, anchor: anchor, after: offset > 0)
      displayOrder.clear()
      NotificationCenter.default.post(name: .parkSiteOrderChanged, object: nil)
    } }
  }

  private func update(_ change: () async throws -> Void) async {
    guard !busy else { return }
    busy = true
    defer { busy = false }
    do {
      try await change()
      rows = try await session.managedSites(library)
      failed = false
    } catch { if !Task.isCancelled { failed = true } }
  }
}

@MainActor private struct CategoryOrderView: View {
  let session: ProgressSession
  let tr: (String) -> String
  let close: () -> Void
  @State private var rows: [String] = []
  @State private var confirm = false
  @State private var failed = false
  @State private var saving = false
  @State private var retry = 0

  var body: some View {
    VStack(alignment: .leading) {
      HStack {
        Button(tr("返回"), action: close)
        Text(tr("分类顺序")).font(.headline)
        Spacer()
        Button(tr("恢复默认顺序")) { confirm = true }
      }.padding()
      if failed { Button(tr("重试")) { retry += 1 } }
      List(Array(rows.enumerated()), id: \.element) { index, id in
        let label = tr(appleCategoryOptions.first { $0.id == id }?.label ?? id)
        HStack {
          Text(label)
          Spacer()
          Button { move(index, -1) } label: { Image(systemName: "arrow.up") }
            .accessibilityLabel(Text(tr("上移") + " " + label)).disabled(index == 0)
          Button { move(index, 1) } label: { Image(systemName: "arrow.down") }
            .accessibilityLabel(Text(tr("下移") + " " + label)).disabled(index + 1 == rows.count)
        }.buttonStyle(.borderless)
      }
    }
    .disabled(saving).interactiveDismissDisabled(saving)
    .task(id: retry) {
      do {
        let prefs = try await session.privacySettings()
        rows = discoveryDomains(order: prefs.domainOrder ?? [], dimension: "all", media: "all")
        failed = false
      } catch { if !Task.isCancelled { failed = true } }
    }
    .alert(tr("恢复默认顺序"), isPresented: $confirm) {
      Button(tr("取消"), role: .cancel) {}
      Button(tr("恢复")) { save(discoveryDomains(order: [], dimension: "all", media: "all")) }
    } message: { Text(tr("将替换当前手动排列的分类顺序，是否继续？")) }
  }

  private func move(_ index: Int, _ offset: Int) {
    guard rows.indices.contains(index + offset) else { return }
    var updated = rows
    updated.swapAt(index, index + offset)
    save(updated)
  }

  private func save(_ order: [String]) {
    guard !saving else { return }
    saving = true
    Task {
      defer { saving = false }
      do {
        let json = String(decoding: try JSONEncoder().encode(order), as: UTF8.self)
        try await session.setPreference("domainOrder", json: json)
        rows = order
        NotificationCenter.default.post(name: .parkSiteOrderChanged, object: nil)
        failed = false
      } catch { failed = true }
    }
  }
}

@MainActor struct SourceResults: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let close: () -> Void
  @State private var input = ""
  @State private var keyword = ""
  @State private var works: [SiteWork] = []
  @State private var selected: SiteWork?
  @State private var page = 1
  @State private var cursor: String?
  @State private var next = false
  @State private var loading = true
  @State private var catalog: SiteBrowseOptions?
  @State private var chosenFeed: String?
  @State private var filters: [String: [String: PluginOptionValue]] = [:]
  @State private var limits: [String: [String: [String: String]]] = [:]
  @State private var showFilters = false
  @State private var settingsChanged = false
  @State private var showLogin = false
  @State private var problem: String?
  @State private var attempt = 0
  @State private var delivered: Int?
  @StateObject private var prompt = SitePrompt()
  @StateObject private var filterPrompt = SitePrompt()

  private var feed: String? { chosenFeed ?? site.entryFeed?.key }
  private var scope: String { keyword.isEmpty ? "feed:" + (feed ?? "") : "search" }
  private var commonRules: [String: SiteCommonRule] {
    if !keyword.isEmpty { return catalog?.searchCommonFilters ?? [:] }
    return catalog?.feeds.first { $0.key == feed }?.commonFilters ?? [:]
  }
  private var sharedDimensions: Set<String> {
    guard keyword.isEmpty, let catalog, catalog.feeds.count > 1 else { return [] }
    return Set(commonRules.keys.filter { dimension in
      catalog.feeds.allSatisfy { $0.commonFilters?[dimension] == commonRules[dimension] }
    })
  }
  private var definitions: [PluginOption] {
    if !keyword.isEmpty { return catalog?.searchFilters ?? [] }
    return catalog?.feeds.first { $0.key == feed }?.options ?? []
  }

  var body: some View {
    Group {
      if let selected {
        SiteDetails(
          session: session, library: library, site: site, workId: selected.id,
          preview: selected, locale: locale, incognito: incognito, tr: tr
        ) { self.selected = nil }.id(selected.id)
      } else {
        VStack(alignment: .leading) {
          HStack {
            Button(tr("返回"), action: close)
            Text(site.name ?? site.key).font(.headline)
            if catalog?.hasSettings == true { Button(tr("站点设置")) { showLogin = true } }
          }.padding(.horizontal)
          if site.search == true {
            HStack {
              TextField(tr("搜索"), text: $input).onSubmit(submit)
              Button(tr("搜索"), action: submit)
            }.padding(.horizontal)
          }
          HStack {
            if keyword.isEmpty, let catalog, catalog.feeds.count > 1 {
              Menu {
                ForEach(catalog.feeds) { channel in
                  Button(catalog.localized(channel.label ?? channel.key)) {
                    guard channel.key != feed else { return }
                    chosenFeed = channel.key
                    restartResults()
                  }
                }
              } label: {
                Text(catalog.feeds.first { $0.key == feed }.map { catalog.localized($0.label ?? $0.key) } ?? tr("全部站点"))
              }
            }
            if !definitions.isEmpty || !commonRules.isEmpty { Button(tr("筛选")) { showFilters = true } }
          }
          List {
            ForEach(works) { item in
              if site.detail { Button(item.title) { selected = item } } else { Text(item.title) }
            }
            if let problem {
              Text(tr(problem))
              if problem == "这个站点需要登录后才能浏览" { Button(tr("去登录")) { showLogin = true } }
              Button(tr("重试")) { attempt += 1 }
            } else if loading {
              ProgressView()
            } else if next {
              Button(tr("加载更多")) {
                loading = true
                page += 1
                attempt += 1
              }
            } else if works.isEmpty {
              Text(tr("暂无内容"))
            }
          }
        }
      }
    }
    .task(id: attempt) {
      if delivered == attempt { return }
      loading = true
      problem = nil
      do {
        if catalog == nil {
          let options = try await session.browseOptions(library, site: site.key, locale: locale)
          try Task.checkCancellation()
          catalog = options
        }
        if keyword.isEmpty && feed == nil {
          loading = false
          delivered = attempt
          return
        }
        let result = try await prompt.perform { ticket in
          try await session.sitePage(
            library, site: site, keyword: keyword, page: page, cursor: cursor, feedKey: feed,
            values: filters[scope] ?? SiteBrowseOptions.defaults(definitions),
            common: try SiteCommonRule.validated(limits[scope] ?? [:], rules: commonRules), confirmation: ticket)
        }
        try Task.checkCancellation()
        var seen = Set(works.map(\.id))
        works.append(contentsOf: result.items.filter { seen.insert($0.id).inserted })
        cursor = result.cursor
        next = result.hasNext
        delivered = attempt
      } catch { if !Task.isCancelled { problem = siteMessage(error) } }
      if !Task.isCancelled { loading = false }
    }.siteConfirmation(prompt, tr: tr)
    .sheet(isPresented: $showFilters) {
      if let catalog {
        PluginFilterForm(localize: catalog.localized, definitions: definitions,
                         values: filters[scope] ?? SiteBrowseOptions.defaults(definitions), tr: tr,
                         query: { option, values in
                           SiteFilterQuery(source: site.key, feed: keyword.isEmpty ? feed : nil,
                                           keyword: keyword, option: option, values: values, definitions: definitions)
                         }, load: { query in
                           try await filterPrompt.perform { ticket in
                             try await session.filterChoices(library, query: query, confirmation: ticket)
                           }
                         }, commonRules: commonRules, sharedDimensions: sharedDimensions,
                         limits: limits[scope] ?? [:]) { values, boundaries in
          filters[scope] = values
          limits[scope] = boundaries
          restartResults()
        }.siteConfirmation(filterPrompt, tr: tr)
      }
    }
    .sheet(isPresented: $showLogin, onDismiss: {
      if settingsChanged { settingsChanged = false; filters = [:]; limits = [:]; restartResults() }
    }) {
      SiteSettingsPage(session: session, library: library, site: site, locale: locale, tr: tr,
                       changed: { settingsChanged = true }, signedIn: { showLogin = false })
    }
  }

  private func submit() {
    let next = input.trimmingCharacters(in: .whitespacesAndNewlines)
    if next != keyword, let catalog, var values = filters["search"] {
      let dependent = Set(catalog.searchFilters.filter { $0.keywordDependent == true }.map(\.key))
      SiteBrowseOptions.reset(dependent, in: catalog.searchFilters, values: &values)
      filters["search"] = values
    }
    keyword = next
    restartResults()
  }

  private func restartResults() {
    works = []
    cursor = nil
    next = false
    page = 1
    loading = true
    attempt += 1
  }
}

@MainActor struct StoredSiteDetails: View {
  let session: ProgressSession
  let work: LibraryWork
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @State private var site: InstalledSite?
  @State private var failed = false
  @State private var retry = 0

  var body: some View {
    Group {
      if let site, let library = InstalledSources.library {
        SiteDetails(
          session: session, library: library, site: site, workId: work.workId, preview: nil,
          locale: locale, incognito: incognito, tr: tr, close: back)
      } else {
        VStack {
          Button(tr("返回"), action: back)
          Text(work.title).font(.headline)
          if failed { Button(tr("站点加载失败，请重试")) { retry += 1 } } else { ProgressView() }
        }
      }
    }.task(id: retry) {
      if site != nil { return }
      failed = false
      do {
        guard let library = InstalledSources.library,
          let found = try await session.sites(library).first(where: {
            $0.key == work.source && $0.detail
          })
        else { throw SiteReadError.malformed }
        site = found
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}

@MainActor struct SiteDetails: View {
  @EnvironmentObject private var recorder: BroadcastRecorder
  @Environment(\.blurCovers) private var blurCovers
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let workId: String
  let preview: SiteWork?
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  var startChapter: String? = nil
  let close: () -> Void
  @State private var detail: SiteWork?
  @State private var work: LibraryWork?
  @State private var chapter: ChapterKey?
  @State private var resumedChapter: String?
  @State private var progressReady = false
  @State private var progressFailed = false
  @State private var progressAttempt = 0
  @State private var article: [SiteArticleBlock] = []
  @State private var selectedBlock: Int?
  @State private var relatedWork: SiteWork?
  @State private var playingGame = false
  @State private var browsingVoice = false
  @State private var downloadSheet = false
  @State private var saving = false
  @State private var saveFailed = false
  @State private var cover: CGImage?
  @State private var imageFailed = false
  @State private var showLogin = false
  @State private var problem: String?
  @State private var attempt = 0
  @State private var imageAttempt = 0
  @StateObject private var prompt = SitePrompt()
  var body: some View {
    if let index = selectedBlock, let detail {
      ArticleGallery(session: session, library: library, site: site.key, work: detail,
        blocks: article, selection: index, tr: tr, select: { selectedBlock = $0 })
    } else if browsingVoice, let detail {
      VoiceDirectoryPage(session: session, library: library, site: site.key, work: detail,
        folder: detail.voiceRoot, incognito: incognito, tr: tr, back: { browsingVoice = false })
    } else if playingGame {
      SiteGame(session: session, library: library, site: site.key, work: workId, tr: tr) {
        playingGame = false
      }
    } else if let chapter, let work {
      if work.media == "novel" {
        NovelReader(
          session: session, key: chapter, work: work, incognito: incognito, locale: locale,
          showBack: true, tr: tr,
          back: { self.chapter = nil }, library: library
        ).id(chapter)
      } else if work.media == "video" {
        MovieReader(
          key: chapter,
          title: work.chapters.first { $0.id == chapter.chapterId }?.title ?? work.title,
          incognito: incognito,
          showBack: true, tr: tr, back: { self.chapter = nil }, library: library
        ).id(chapter)
      } else {
        PageReader(
          incognito: incognito, tr: tr, session: session, key: chapter, work: work, showBack: true,
          back: { self.chapter = nil }, library: library
        ).id(chapter)
      }
    } else {
      details
    }
  }

  private var details: some View {
    VStack(alignment: .leading) {
      Button(tr("返回"), action: close).padding(.horizontal)
      if let detail {
        ScrollView {
          VStack(alignment: .leading, spacing: 12) {
            Text(detail.title).font(.title)
            if let subtitle = detail.subtitle { Text(subtitle) }
            if let cover {
              Image(decorative: cover, scale: 1).resizable().scaledToFit().frame(maxHeight: 360)
                .blur(radius: blurCovers ? 18 : 0).clipped()
            } else if imageFailed {
              Button(tr("重试")) { imageAttempt += 1 }
            }
            if let description = detail.description { Text(description) }
            if !article.isEmpty {
              ArticleBody(session: session, library: library, site: site.key, title: detail.title,
                blocks: article, tr: tr, select: { selectedBlock = $0 })
            }
            if let work {
              if let first = work.chapters.first {
                let resume = work.chapters.first { $0.id == resumedChapter }
                Button(tr(work.media == "video" ? "播放" : resume == nil ? "开始阅读" : "继续阅读")) {
                  chapter = ChapterKey(source: site.key, workId: work.workId, chapterId: (resume ?? first).id)
                }.disabled(!progressReady)
                .task(id: progressAttempt) {
                  progressReady = false
                  progressFailed = false
                  do {
                    let saved = try await session.lastReadChapter(source: site.key, work: work.workId)
                    try Task.checkCancellation()
                    resumedChapter = saved
                    progressReady = true
                  } catch { if !Task.isCancelled { progressFailed = true } }
                }
                if progressFailed { Button(tr("重试")) { progressAttempt += 1 } }
              }
              if work.media == "tavern" {
                TavernResourceActions(session: session, library: library, site: site.key, item: detail, tr: tr)
                  .id(work.libraryID)
              }
              if work.contentDomain == "live", work.media == "video", site.nativeVideo {
                if recorder.contains(work.libraryID) {
                  Button(tr("停止录制")) { recorder.stop(work.libraryID) }
                    .disabled(recorder.isStopping(work.libraryID))
                } else {
                  Button(tr("开始录制")) { recorder.start(library, work: work) }
                }
                if recorder.failures.contains(work.libraryID) {
                  Text(tr("录制失败，已保存已有内容"))
                }
              }
              Button(tr(work.favorite ? "取消应用内收藏" : "应用内收藏")) {
                saving = true
                saveFailed = false
                Task {
                  defer { saving = false }
                  do {
                    self.work = try await session.openSiteWork(
                      library, site: site, item: detail, incognito: incognito,
                      favorite: !work.favorite)
                  } catch { if !Task.isCancelled { saveFailed = true } }
                }
              }.disabled(saving)
              if saveFailed { Text(tr("保存失败，请重试")) }
                if !work.chapters.isEmpty && (work.media == "comic" || work.media == "novel" || work.media == "video" && site.nativeVideo) {
                  Button(tr("下载")) { downloadSheet = true }
                    .sheet(isPresented: $downloadSheet) {
                      SiteDownload(session: session, library: library, detail: detail, work: work, tr: tr)
                    }
                }
              if work.media == "voice" {
                Button(tr(detail.voiceRoot?.type == "directory" ? "打开目录" : "播放")) {
                  browsingVoice = true
                }
              }
              if work.contentDomain == "onlineGame" && work.media == "game" {
                Button(tr("开始游戏")) { playingGame = true }
              }
              ForEach(work.chapters, id: \.id) { row in
                Button(row.title) {
                  chapter = ChapterKey(source: site.key, workId: work.workId, chapterId: row.id)
                }
              }
            }
            if site.comments && detail.allowsComments {
              DetailEntriesPanel(session: session, library: library, site: site, work: detail, kind: .comments,
                tr: tr, select: { relatedWork = $0 })
            }
            if site.related || !detail.recommendations.isEmpty {
              DetailEntriesPanel(session: session, library: library, site: site, work: detail, kind: .related,
                tr: tr, select: { relatedWork = $0 })
            }
          }.frame(maxWidth: .infinity, alignment: .leading).padding()
        }
        .task(id: imageAttempt) {
          guard let url = detail.cover ?? preview?.cover, !url.isEmpty else { return }
          imageFailed = false
          do {
            let asset = try await session.siteImage(library, site: site.key, address: url)
            let pixels = try await decodedPage(asset, maxSize: 1200)
            try Task.checkCancellation()
            cover = pixels
          } catch { if !Task.isCancelled { imageFailed = true } }
        }
      } else if let problem {
        Text(tr(problem))
              if problem == "这个站点需要登录后才能浏览" { Button(tr("去登录")) { showLogin = true } }
        Button(tr("重试")) { attempt += 1 }
      } else {
        ProgressView()
      }
    }.sheet(item: $relatedWork) { selected in
      AnyView(SiteDetails(session: session, library: library, site: site, workId: selected.id, preview: selected,
        locale: locale, incognito: incognito, tr: tr, close: { relatedWork = nil }).id(selected.id))
    }.task(id: attempt) {
      guard detail == nil else { return }
      problem = nil
      do {
        let found = try await prompt.perform {
          try await session.siteDetail(library, site: site.key, id: workId, preview: preview, confirmation: $0)
        }
        let opened = try await session.openSiteWork(
          library, site: site, item: found, incognito: incognito)
        if opened.media == "article" { article = try found.articleBlocks() }
        work = opened
        detail = found
        if let startChapter, work?.chapters.contains(where: { $0.id == startChapter }) == true {
          chapter = ChapterKey(source: site.key, workId: workId, chapterId: startChapter)
        }
      } catch { if !Task.isCancelled { problem = siteMessage(error) } }
    }.siteConfirmation(prompt, tr: tr)
    .sheet(isPresented: $showLogin) {
      SiteSettingsPage(session: session, library: library, site: site, locale: locale, tr: tr, signedIn: {
        showLogin = false; attempt += 1
      })
    }
  }
}

@MainActor private struct DetailEntriesPanel: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let work: SiteWork
  let kind: DetailSectionKind
  let tr: (String) -> String
  let select: (SiteWork) -> Void
  var parent: SiteComment? = nil
  @State private var selectedReply: SiteComment?
  @State private var sort = "default"
  @State private var orders: [SiteCommentOrder] = []
  @State private var open = false
  @State private var entries: [DetailEntry] = []
  @State private var nextPage: Int? = 1
  @State private var cursor: PluginOptionValue?
  @State private var attempt = 0
  @State private var delivered: Int?
  @State private var loading = false
  @State private var problem: String?
  @StateObject private var prompt = SitePrompt()

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      if parent == nil {
        Button(tr(kind == .comments ? "评论" : "相关作品")) { open.toggle() }
      }
      if open || parent != nil {
        VStack(alignment: .leading, spacing: 8) {
          if parent == nil && !orders.isEmpty {
            Picker(tr("排序"), selection: Binding(get: {
              sort == "default" ? orders[0].value : sort
            }, set: { value in
              sort = value
              entries.removeAll()
              nextPage = 1
              cursor = nil
              delivered = nil
              attempt += 1
            })) {
              ForEach(orders) { order in Text(tr(order.label)).tag(order.value) }
            }
          }
          ForEach(entries) { entry in
            switch entry {
            case .work(let row): Button(row.title) { select(row) }
            case .comment(let row):
              if let name = row.author?.name, !name.isEmpty { Text(name).font(.headline) }
              ForEach(row.body.indices, id: \.self) { index in
                let part = row.body[index]
                if part.type == "image", let address = part.url {
                  ArticlePicture(session: session, library: library, site: site.key, address: address,
                    title: part.text ?? "", maximum: 1000, tr: tr)
                } else if let text = part.text, !text.isEmpty { Text(text) }
                else if part.type == "link", let address = part.url { Text(address) }
              }
              if row.repliesAvailable == true {
                Button(tr("查看回复")) { selectedReply = row }
                  .accessibilityHint(row.author?.name ?? "")
              }
            }
          }
          if loading { ProgressView() }
          else if let problem {
            Text(tr(problem))
            Button(tr("重试")) { attempt += 1 }
          } else if delivered != nil {
            if entries.isEmpty { Text(tr(kind == .comments ? "暂无评论" : "暂无内容")) }
            if nextPage != nil { Button(tr("加载更多")) { attempt += 1 } }
          }
        }.task(id: attempt) { await load() }
      }
    }.siteConfirmation(prompt, tr: tr)
    .sheet(item: $selectedReply) { row in
      VStack(alignment: .leading) {
        Button(tr("返回")) { selectedReply = nil }
        ScrollView {
          DetailEntriesPanel(session: session, library: library, site: site, work: work, kind: .comments,
            tr: tr, select: select, parent: row)
        }
      }.padding()
    }
  }

  private func load() async {
    guard delivered != attempt, let page = nextPage else { return }
    if let parent, delivered == nil && entries.isEmpty {
      entries = (parent.replies ?? []).map(DetailEntry.comment)
    }
    if kind == .related && delivered == nil {
      var seen = Set<String>()
      entries = work.recommendations.filter { seen.insert($0.id).inserted }.map(DetailEntry.work)
      if !site.related { nextPage = nil; delivered = attempt; return }
    }
    loading = true
    problem = nil
    do {
      let result = try await prompt.perform { consent in
        try await session.detailEntries(library, site: site, work: work, section: kind, page: page,
          cursor: cursor, confirmation: consent, sort: sort, parent: parent?.id)
      }
      try Task.checkCancellation()
      if result.reset { entries.removeAll() }
      if !result.orders.isEmpty { orders = result.orders }
      var seen = Set(entries.map(\.id))
      let fresh = result.entries.filter { seen.insert($0.id).inserted }
      entries.append(contentsOf: fresh)
      nextPage = page > 1 && fresh.isEmpty && !result.entries.isEmpty ? nil : result.nextPage
      cursor = result.cursor
      delivered = attempt
    } catch { if !Task.isCancelled { problem = siteMessage(error) } }
    if !Task.isCancelled { loading = false }
  }
}
