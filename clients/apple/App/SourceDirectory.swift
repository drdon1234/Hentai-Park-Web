import ParkCore
import SwiftUI
import ImageIO
#if os(macOS)
import AppKit
#else
import UIKit
#endif

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
  @State private var options: InstalledSite?
  @State private var searchOnly = false
  @State private var hidden = Set<String>()
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
        tr: tr, startInSearch: searchOnly
      ) { self.chosen = nil; searchOnly = false }.id(chosen.id)
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
              searchOnly = false; chosen = site
            } label: {
              VStack(alignment: .leading) {
                Text(site.name ?? site.key)
                if site.unavailable == true { Text(tr("站点加载失败，请重试")).font(.caption) }
              }
            }.contextMenu {
              Button(tr("站点设置")) { options = site }
              if site.search == true { Button(tr("搜索")) { searchOnly = true; chosen = site } }
              Toggle(tr("显示该站点的作品并自动加载"), isOn: Binding(get: { !hidden.contains(site.key) }, set: { value in
                Task {
                  do { hidden = Set(try await session.setHomeSite(site.key, visible: value)) }
                  catch { failed = true }
                }
              }))
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
          hidden = Set(try await session.privacySettings().homeHiddenSources ?? [])
        } catch { if !Task.isCancelled { failed = true } }
      }.sheet(item: $options) { site in
        if let library = InstalledSources.library {
          SiteSettingsPage(session: session, library: library, site: site, locale: locale, tr: tr,
                           changed: { retry += 1 }, signedIn: { options = nil })
        }
      }
    }
  }
}

@MainActor struct ReferenceDirectory: View {
  let session: ProgressSession
  let library: SourceLibrary
  let domain: String
  let sites: [InstalledSite]
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @State private var input = ""
  @State private var keyword: String?
  @State private var source: InstalledSite?

  var body: some View {
    if let keyword {
      AggregateSearchPage(session: session, locale: locale, incognito: incognito, tr: tr,
          back: { self.keyword = nil }, initialKeyword: keyword, category: domain, metadataOnly: true)
    } else if let source {
      SourceResults(session: session, library: library, site: source, locale: locale, incognito: incognito,
                    tr: tr, referenceDomain: domain) { self.source = nil }
    } else {
      VStack(alignment: .leading) {
        HStack { Button(tr("返回"), action: back); Text(tr("搜索作品资料")).font(.headline) }
        HStack {
          TextField(tr(domain == "jav" ? "搜索番号、作品名或演员" : "搜索作品名、标签或作者"), text: $input).onSubmit(submit)
            .accessibilityIdentifier("reference-keyword")
          Button(tr("搜索"), action: submit)
        }
        List(sites) { site in Button(site.name ?? site.key) { source = site } }
      }.padding()
    }
  }

  private func submit() {
    let term = input.trimmingCharacters(in: .whitespacesAndNewlines)
    if !term.isEmpty { keyword = term }
  }
}

@MainActor struct ReferenceDetailPage: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let id: String
  let tr: (String) -> String
  let back: () -> Void
  @StateObject private var prompt = SitePrompt()
  @State private var reference: ChartReference?
  @State private var failure: String?
  @State private var attempt = 0

  var body: some View {
    List {
      Button(tr("返回"), action: back)
      if let reference {
        Text(reference.title).font(.headline)
        if let code = reference.code { Text(code) }
        if let description = reference.description { Text(description) }
      } else if let failure {
        Text(tr(failure)); Button(tr("重试")) { attempt += 1 }
      } else { ProgressView() }
    }.task(id: attempt) {
      failure = nil
      do {
        let result = try await prompt.perform { ticket in
          try await session.referenceDetail(library, site: site, referenceID: id, confirmation: ticket)
        }
        try Task.checkCancellation()
        reference = result
      } catch { if !Task.isCancelled { failure = siteMessage(error) } }
    }.siteConfirmation(prompt, tr: tr)
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
          Toggle(tr("启用"), isOn: Binding(get: { row.enabled }, set: { enabled in
            Task { await update {
              try await session.setSiteEnabled(library, key: row.id, enabled: enabled)
              displayOrder.clear()
              NotificationCenter.default.post(name: .parkSiteOrderChanged, object: nil)
            } }
          })).labelsHidden().accessibilityLabel(tr("启用") + " " + row.name)
            .accessibilityIdentifier("site-enabled:" + row.id)
          Button { move(index, -1) } label: { Image(systemName: "arrow.up") }
            .accessibilityLabel(Text(tr("上移") + " " + row.name)).disabled(index == 0)
          Button { move(index, 1) } label: { Image(systemName: "arrow.down") }
            .accessibilityLabel(Text(tr("下移") + " " + row.name)).disabled(index + 1 == rows.count)
            .accessibilityIdentifier("site-down:" + row.id)
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
  var startingTag: DetailSearchTag? = nil
  var startingLink: URL? = nil
  var referenceDomain: String? = nil
  var startInSearch = false
  let close: () -> Void
  @State private var filterSearch = false
  @State private var input = ""
  @State private var keyword = ""
  @State private var works: [SiteWork] = []
  @State private var selected: SiteWork?
  @State private var randomTarget: String?
  @State private var randomRequest: UUID?
  @State private var randomError: String?
  @StateObject private var randomPrompt = SitePrompt()
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
  @State private var albumScope: Data?
  @StateObject private var translationPage = TranslationPage()
  @State private var problem: String?
  @State private var attempt = 0
  @State private var delivered: Int?
  @StateObject private var prompt = SitePrompt()
  @StateObject private var filterPrompt = SitePrompt()

  private var feed: String? { chosenFeed ?? site.entryFeed?.key }
  private var searching: Bool { !keyword.isEmpty || filterSearch || startingLink != nil }
  private var scope: String { !searching ? "feed:" + (feed ?? "") : "search" }
  private var commonRules: [String: SiteCommonRule] {
    if searching { return catalog?.searchCommonFilters ?? [:] }
    return catalog?.feeds.first { $0.key == feed }?.commonFilters ?? [:]
  }
  private var sharedDimensions: Set<String> {
    guard !searching, let catalog, catalog.feeds.count > 1 else { return [] }
    return Set(commonRules.keys.filter { dimension in
      catalog.feeds.allSatisfy { $0.commonFilters?[dimension] == commonRules[dimension] }
    })
  }
  private var definitions: [PluginOption] {
    if searching { return catalog?.searchFilters ?? [] }
    return catalog?.feeds.first { $0.key == feed }?.options ?? []
  }

  var body: some View {
    Group {
      if let target = selected?.id ?? randomTarget {
        if referenceDomain != nil {
          ReferenceDetailPage(session: session, library: library, site: site, id: target, tr: tr) {
            selected = nil; randomTarget = nil
          }.id(target)
        } else { SiteDetails(
          session: session, library: library, site: site, workId: target,
          preview: selected, locale: locale, incognito: incognito, tr: tr
        ) { selected = nil; randomTarget = nil }.id(target) }
      } else {
        VStack(alignment: .leading) {
          HStack {
            Button(tr("返回"), action: close)
            Text(site.name ?? site.key).font(.headline)
            TranslationControls(page: translationPage, tr: tr)
            if catalog?.hasSettings == true { Button(tr("站点设置")) { showLogin = true } }
            if site.album, catalog != nil, startingLink == nil, searching || feed != nil {
              Button(tr("相册")) { openAlbum() }
            }
          }.padding(.horizontal)
          if (site.search == true || referenceDomain != nil && site.metadataSearch) && startingLink == nil {
            HStack {
              TextField(tr("搜索"), text: $input).onSubmit(submit)
                .accessibilityIdentifier("site-keyword")
              Button(tr("搜索"), action: submit)
            }.padding(.horizontal)
          }
          HStack {
            if !searching, let catalog, catalog.feeds.count > 1 {
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
            if startingLink == nil && (!definitions.isEmpty || !commonRules.isEmpty) { Button(tr("筛选")) { showFilters = true } }
            if site.random && startingTag == nil && startingLink == nil {
              Button(tr(randomRequest == nil ? "随机一部" : "取消")) {
                randomError = nil
                randomRequest = randomRequest == nil ? UUID() : nil
              }
              .task(id: randomRequest) {
                guard randomRequest != nil else { return }
                do {
                  let reference = try await randomPrompt.perform { ticket in
                    try await session.randomReference(library, site: site, confirmation: ticket)
                  }
                  let target = try await randomPrompt.perform { ticket in
                    try await session.resolveWorkReference(library, site: site, reference: reference, confirmation: ticket)
                  }
                  try Task.checkCancellation()
                  randomRequest = nil
                  randomTarget = target
                } catch {
                  if !Task.isCancelled { randomError = siteMessage(error); randomRequest = nil }
                }
              }.siteConfirmation(randomPrompt, tr: tr)
            }
          }
          if let randomError { Text(tr(randomError)).font(.caption) }
          List {
            ForEach(works) { item in
              let title = RemoteText(original: item.title, dictionary: nil, session: session, library: library, locale: locale, page: translationPage)
              if (referenceDomain != nil ? site.metadataDetail : site.detail) { Button { selected = item } label: { title } } else { title }
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
        var requestKeyword = keyword
        var requestFilterSearch = filterSearch
        var requestValues = filters[scope] ?? SiteBrowseOptions.defaults(definitions)
        if catalog == nil {
          let options = try await session.browseOptions(library, site: site.key, locale: locale)
          try Task.checkCancellation()
          catalog = options
          if startInSearch && startingTag == nil && attempt == 0 && keyword.isEmpty {
            filterSearch = true
            loading = false
            delivered = attempt
            return
          } else if let startingTag {
            requestKeyword = startingTag.keyword
            requestFilterSearch = !startingTag.filters.isEmpty
            requestValues = SiteBrowseOptions.defaults(options.searchFilters).merging(startingTag.filters) { _, value in value }
            input = requestKeyword
            keyword = requestKeyword
            filterSearch = requestFilterSearch
            filters["search"] = requestValues
          } else {
            let options = searching ? options.searchFilters : options.feeds.first { $0.key == feed }?.options ?? []
            requestValues = filters[scope] ?? SiteBrowseOptions.defaults(options)
          }
        }
        if requestKeyword.isEmpty && !requestFilterSearch && startingLink == nil && feed == nil {
          loading = false
          delivered = attempt
          return
        }
        let result = try await prompt.perform { ticket in
          try await session.sitePage(
            library, site: site, keyword: requestKeyword, page: page, cursor: cursor, feedKey: feed,
            values: requestValues, filterSearch: requestFilterSearch, link: startingLink, referenceDomain: referenceDomain,
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
                           SiteFilterQuery(source: site.key, feed: !searching ? feed : nil,
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
    .sheet(isPresented: Binding(get: { albumScope != nil }, set: { if !$0 { albumScope = nil } })) {
      if let albumScope {
        SiteAlbumPage(session: session, library: library, site: site, scope: albumScope, locale: locale,
                      incognito: incognito, tr: tr, close: { self.albumScope = nil })
      }
    }
  }

  private func openAlbum() {
    let values = filters[scope] ?? SiteBrowseOptions.defaults(definitions)
    do {
      let options = try JSONSerialization.jsonObject(with: JSONEncoder().encode(values))
      let input: [String:Any]
      if searching { input = ["search":["keyword":keyword,"filters":options] as [String:Any]] }
      else if let feed { input = ["feed":feed,"options":options] }
      else { return }
      albumScope = try JSONSerialization.data(withJSONObject: input)
    } catch { problem = siteMessage(error) }
  }

  private func submit() {
    let next = input.trimmingCharacters(in: .whitespacesAndNewlines)
    if next != keyword, let catalog, var values = filters["search"] {
      let dependent = Set(catalog.searchFilters.filter { $0.keywordDependent == true }.map(\.key))
      SiteBrowseOptions.reset(dependent, in: catalog.searchFilters, values: &values)
      filters["search"] = values
    }
    filterSearch = false
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
  let source: String
  let workId: String
  let title: String
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
          session: session, library: library, site: site, workId: workId, preview: nil,
          locale: locale, incognito: incognito, tr: tr, close: back)
      } else {
        VStack {
          Button(tr("返回"), action: back)
          Text(title).font(.headline)
          if failed { Button(tr("站点加载失败，请重试")) { retry += 1 } } else { ProgressView() }
        }
      }
    }.task(id: retry) {
      if site != nil { return }
      failed = false
      do {
        guard let library = InstalledSources.library,
          let found = try await session.sites(library).first(where: {
            $0.key == source && $0.detail
          })
        else { throw SiteReadError.malformed }
        site = found
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}

private struct DetailSection<Content: View>: View {
  let title: String
  @Binding var expanded: Bool
  @ViewBuilder let content: () -> Content
  var body: some View {
    #if os(macOS)
    VStack(alignment: .leading, spacing: 8) {
      Button {
        expanded.toggle()
      } label: {
        HStack(spacing: 6) {
          Image(systemName: expanded ? "chevron.down" : "chevron.right")
            .font(.caption).foregroundStyle(.secondary).accessibilityHidden(true)
          Text(title)
        }.frame(maxWidth: .infinity, alignment: .leading)
          .contentShape(Rectangle())
      }
      .buttonStyle(.plain)
      .accessibilityAddTraits(expanded ? .isSelected : [])
      if expanded { content() }
    }
    #else
    DisclosureGroup(title, isExpanded: $expanded, content: content)
    #endif
  }
}

private struct FavoriteFolder: Identifiable, Hashable {
  let id: Int64
  let name: String
}

private struct FavoriteFolderSelection: View {
  let folders: [FavoriteFolder]
  let saving: Bool
  let failed: Bool
  let tr: (String) -> String
  let cancel: () -> Void
  let save: ([Int64]) -> Void
  @State private var selected = Set<Int64>()

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      Text(tr("收藏到")).font(.headline)
      List(folders) { folder in
        Toggle(folder.name, isOn: Binding(get: { selected.contains(folder.id) }, set: { enabled in
          if enabled { selected.insert(folder.id) } else { selected.remove(folder.id) }
        }))
      }
      #if os(macOS)
      .frame(minHeight: 96, idealHeight: CGFloat(min(max(folders.count, 1), 8) * 32 + 16))
      #endif
      if failed { Text(tr("保存失败，请重试")) }
      HStack {
        Button(tr("取消"), action: cancel)
        Button(tr("确定")) { save(selected.sorted()) }
      }
    }.padding().disabled(saving).interactiveDismissDisabled(saving)
  }
}

private enum DetailDestination: Hashable, Identifiable {
  case author(String)
  case search(DetailSearchTag)
  case editions(String)
  case similar(source: String, id: String, title: String)
  case existing(source: String, id: String, title: String)
  case category([FavoriteFolder])
  var id: Self { self }
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
  var openedDetail: SiteWork? = nil
  var openedEvent: String? = nil
  let close: () -> Void
  @State private var detail: SiteWork?
  @State private var work: LibraryWork?
  @State private var chapter: ChapterKey?
  @State private var previewPage: Int?
  @State private var previewsExpanded = false
  @State private var previewPager = PreviewPager()
  @State private var previewAttempt = 0
  @State private var lastPreviewAttempt = 0
  @StateObject private var translationPage = TranslationPage()
  @Environment(\.autoTranslateContent) private var automaticTranslation
  @Environment(\.privacyObscured) private var translationObscured
  @State private var previewBusy = false
  @State private var previewError: String?
  @State private var resumedChapter: String?
  @State private var progressReady = false
  @State private var progressFailed = false
  @State private var progressAttempt = 0
  @State private var article: [SiteArticleBlock] = []
  @State private var selectedBlock: Int?
  @State private var relatedWork: SiteWork?
  @State private var destination: DetailDestination?
  @State private var dictionary: SearchDictionary?
  @State private var dictionaryRevision = 0
  @State private var tagsExpanded = false
  @State private var playingGame = false
  @State private var browsingGameResources = false
  @State private var browsingVoice = false
  @State private var downloadSheet = false
  @State private var saving = false
  @State private var saveFailed = false
  @State private var favoriteRevision = 0
  @State private var favoriteReadFailed = false
  @State private var cover: CGImage?
  @State private var imageFailed = false
  @State private var showLogin = false
  @State private var problem: String?
  @State private var attempt = 0
  @State private var imageAttempt = 0
  @StateObject private var prompt = SitePrompt()
  var body: some View {
    content.onReceive(NotificationCenter.default.publisher(for: .parkFavoritesChanged)) { event in
      guard let ids = event.object as? Set<LibraryWorkID>, !ids.isEmpty else { return }
      if work.map({ ids.contains($0.libraryID) }) ?? true { favoriteRevision += 1 }
    }
  }

  @ViewBuilder private var content: some View {
    if let index = selectedBlock, let detail {
      ArticleGallery(session: session, library: library, site: site.key, work: detail,
        blocks: article, selection: index, tr: tr, select: { selectedBlock = $0 })
    } else if browsingVoice, let detail {
      VoiceDirectoryPage(session: session, library: library, site: site.key, work: detail,
        folder: detail.voiceRoot, incognito: incognito, tr: tr, back: { browsingVoice = false })
    } else if browsingGameResources, let work {
      GameResourcePage(session: session, library: library, work: work, tr: tr) { browsingGameResources = false }
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
          back: { self.chapter = nil; previewPage = nil }, library: library, initialPage: previewPage,
          changeChapter: { next in previewPage = nil; self.chapter = next }
        ).id(chapter)
      }
    } else {
      details
    }
  }

  private func changeFavorite(to favorite: Bool, checkSimilar: Bool, categories: [Int64]? = nil) {
    guard let detail, !saving else { return }
    saving = true
    saveFailed = false
    Task { @MainActor in
      defer { saving = false }
      do {
        if favorite, checkSimilar,
          let match = try await session.similarFavorite(source: site.key, id: detail.id, title: detail.title) {
          destination = .similar(source: match.source, id: match.id, title: match.title)
          return
        }
        if favorite, categories == nil, try await session.favoritePreferences().askCategoryOnFavorite == true {
          let folders = try await session.shelfCategories().map { FavoriteFolder(id: $0.id, name: $0.name) }
          if !folders.isEmpty { destination = .category(folders); return }
        }
        self.work = try await session.openSiteWork(library, site: site, item: detail, incognito: incognito,
          favorite: favorite, categories: categories)
        destination = nil
      } catch { if !Task.isCancelled { saveFailed = true } }
    }
  }

  private func searchDetail(_ tag: DetailSearchTag) {
    destination = .search(tag)
  }

  private func tagRow(_ tag: DetailSearchTag, link: String? = nil, isAuthor: Bool = false) -> some View {
    DetailTagRow(session: session, library: library, site: site, tag: tag, locale: locale,
      localText: dictionary?.translation(name: tag.name, namespace: tag.namespace), tr: tr, translationPage: translationPage,
      isAuthor: isAuthor, canOpen: link != nil || site.search == true) {
        if let link { destination = .author(link) } else { searchDetail(tag) }
      }
  }

  private var details: some View {
    VStack(alignment: .leading) {
      HStack {
        Button(tr("返回"), action: close)
        Spacer()
        TranslationControls(page: translationPage, tr: tr)
      }.padding(.horizontal)
      if let detail {
        ScrollView {
          VStack(alignment: .leading, spacing: 12) {
            remote(detail.title).font(.title)
            if let subtitle = detail.subtitle { remote(subtitle) }
            if let cover {
              Image(decorative: cover, scale: 1).resizable().scaledToFit().frame(maxHeight: 360)
                .blur(radius: blurCovers ? 18 : 0).clipped()
            } else if imageFailed {
              Button(tr("重试")) { imageAttempt += 1 }
            }
            if let description = detail.description { remote(description) }
            if !detail.variants.isEmpty || detail.workCode?.isEmpty == false {
              Text(tr("其他版本")).font(.headline)
              ForEach(detail.variants) { variant in
                Button(variant.editions?.isEmpty == false ? variant.editions!.joined(separator: " · ") : variant.title) {
                  relatedWork = variant
                }
              }
              if let code = detail.workCode, !code.isEmpty {
                if site.search == true {
                  Button(tr("站内查找版本")) { searchDetail(DetailSearchTag(name: code)) }
                }
                if site.category != nil {
                  Button(tr("跨站查找版本")) { destination = .editions(code) }
                }
              }
            }
            if let authors = detail.authors, !authors.isEmpty, !detail.searchTags.contains(where: { authors.contains($0.name) }) {
              Text(tr("作者")).font(.headline)
              ForEach(Array(authors.enumerated()), id: \.offset) { _, name in
                let tag = DetailSearchTag(name: name, keyword: detail.authorSearch?.keyword,
                  filters: detail.authorSearch?.filters ?? [:])
                tagRow(tag, link: detail.authorSearch?.url, isAuthor: true)
              }
            }
            if !detail.searchTags.isEmpty {
              DetailSection(title: tr("标签"), expanded: $tagsExpanded) {
                if (translationPage.override ?? automaticTranslation) && !translationObscured {
                  TagPackGuide(session: session, library: library, key: TagPackKey(source: site.key, locale: locale),
                               name: site.name ?? site.key, tr: tr)
                }
                ForEach(Array(detail.searchTags.enumerated()), id: \.offset) { _, tag in
                  tagRow(tag)
                }
              }
            }
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
                changeFavorite(to: !work.favorite, checkSimilar: true)
              }.disabled(saving || favoriteReadFailed)
              .task(id: favoriteRevision) {
                guard favoriteRevision > 0 else { return }
                do {
                  let saved = try await session.isFavorite(source: work.source, work: work.workId)
                  try Task.checkCancellation()
                  self.work?.favorite = saved
                  favoriteReadFailed = false
                } catch { if !Task.isCancelled { favoriteReadFailed = true } }
              }
              if favoriteReadFailed { Button(tr("重试")) { favoriteRevision += 1 } }
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
              if site.gameResources {
                Button(tr("查看游戏资源")) { browsingGameResources = true }
              }
              SiteChapterDirectory(session: session, library: library, detail: detail, work: work, tr: tr,
                                   canDownload: work.media == "comic" || work.media == "novel" || work.media == "video" && site.nativeVideo) { row in
                  chapter = ChapterKey(source: site.key, workId: work.workId, chapterId: row.id)
              }
              if work.media == "comic", let first = work.chapters.first, !previewPager.items.isEmpty || previewPager.hasNext {
                DetailSection(title: tr("预览"), expanded: $previewsExpanded) {
                  let tiles = ForEach(Array(previewPager.items.enumerated()), id: \.offset) { ordinal, item in
                    let page = item.index.flatMap { $0 >= 0 ? $0 : nil } ?? ordinal
                    Button {
                      let target = work.chapters.first { $0.id == resumedChapter } ?? first
                      previewPage = page
                      chapter = ChapterKey(source: site.key, workId: work.workId, chapterId: target.id)
                    } label: {
                      VStack {
                        DetailThumbnail(session: session, library: library, source: site.key, item: item)
                        Text(String(UInt(page) + 1))
                      }
                    }.disabled(!progressReady)
                    .accessibilityLabel(tr("第 {value0} 页").replacingOccurrences(of: "{value0}", with: String(UInt(page) + 1)))
                    .onAppear {
                      if ordinal == previewPager.items.count - 1 && previewPager.automatic && previewPager.hasNext && !previewBusy && previewError == nil {
                        previewAttempt += 1
                      }
                    }
                  }
                  #if os(macOS)
                  ScrollView(.horizontal) {
                    LazyHStack(alignment: .top, spacing: 8) { tiles }
                  }.frame(height: 124)
                  #else
                  LazyVGrid(columns: [GridItem(.adaptive(minimum: 82, maximum: 140))]) { tiles }
                  #endif
                  if previewPager.hasNext {
                    if let previewError { Text(tr(previewError)) }
                    else if previewPager.nextRemotePage > 1 && !previewPager.automatic {
                      Text(tr("本页没有新增预览，可继续加载")).font(.caption).foregroundColor(.secondary)
                    }
                    Button(tr(previewError == nil ? "加载更多预览" : "重试")) {
                      previewPager.startManually()
                      previewAttempt += 1
                    }.disabled(previewBusy)
                  }
                }.task(id: previewAttempt) {
                  guard previewAttempt != lastPreviewAttempt && previewsExpanded else { return }
                  lastPreviewAttempt = previewAttempt
                  await morePreviews()
                }
              }
            }
            if site.comments && detail.allowsComments {
              DetailEntriesPanel(session: session, library: library, site: site, work: detail, kind: .comments,
                locale: locale, incognito: incognito, tr: tr, select: { relatedWork = $0 })
            }
            if site.related || !detail.recommendations.isEmpty {
              DetailEntriesPanel(session: session, library: library, site: site, work: detail, kind: .related,
                locale: locale, incognito: incognito, tr: tr, select: { relatedWork = $0 })
            }
          }.frame(maxWidth: .infinity, alignment: .leading).padding()
        }
        .task(id: "\(site.key)/\(locale)/\(dictionaryRevision)") {
          let result = try? await session.tagDictionary(source: site.key, locale: locale)
          if !Task.isCancelled { dictionary = result }
        }
        .onReceive(NotificationCenter.default.publisher(for: .tagPackChanged)) { notification in
          if notification.object as? TagPackKey == TagPackKey(source: site.key, locale: locale) { dictionaryRevision += 1 }
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
    }.sheet(item: $destination) { target in
      switch target {
      case .similar(let source, let id, let title):
        VStack(alignment: .leading, spacing: 12) {
          Text(tr("书库中已有相似作品")).font(.headline)
          Text("「\(title)」（\(source)）")
          Button(tr("打开已有")) { destination = .existing(source: source, id: id, title: title) }
          Button(tr("仍然收藏")) { changeFavorite(to: true, checkSimilar: false) }
          Button(tr("取消")) { destination = nil }
          if saveFailed { Text(tr("保存失败，请重试")) }
        }.padding().disabled(saving)
      case .existing(let source, let id, let title):
        AnyView(StoredSiteDetails(session: session, source: source, workId: id, title: title, locale: locale,
          incognito: incognito, tr: tr, back: { destination = nil }))
      case .category(let folders):
        FavoriteFolderSelection(folders: folders, saving: saving, failed: saveFailed, tr: tr,
          cancel: { destination = nil }, save: { changeFavorite(to: true, checkSimilar: false, categories: $0) })
      case .author(let link):
        AnyView(ContentLinkDestination(session: session, library: library, site: site, address: link, locale: locale,
          incognito: incognito, tr: tr, close: { destination = nil }))
      case .search(let tag):
        AnyView(SourceResults(session: session, library: library, site: site, locale: locale, incognito: incognito,
          tr: tr, startingTag: tag, close: { destination = nil }))
      case .editions(let code):
        AnyView(AggregateSearchPage(session: session, locale: locale, incognito: incognito, tr: tr,
          back: { destination = nil }, initialKeyword: code, category: site.category))
        #if os(macOS)
        .frame(minWidth: 420, idealWidth: 560, minHeight: 320, idealHeight: 440)
        #endif
      }
    }.sheet(item: $relatedWork) { selected in
      AnyView(SiteDetails(session: session, library: library, site: site, workId: selected.id, preview: selected,
        locale: locale, incognito: incognito, tr: tr, close: { relatedWork = nil }).id(selected.id))
    }.task(id: attempt) {
      guard detail == nil else { return }
      problem = nil
      do {
        let found: SiteWork
        if let openedDetail { found = openedDetail }
        else {
          found = try await prompt.perform {
            try await session.siteDetail(library, site: site.key, id: workId, preview: preview, confirmation: $0)
          }
        }
        let opened = try await session.openSiteWork(
          library, site: site, item: found, incognito: incognito)
        if opened.media == "article" { article = try found.articleBlocks() }
        try Task.checkCancellation()
        if let openedEvent, startChapter == nil || opened.chapters.contains(where: { $0.id == startChapter }) {
          try await session.readSubscriptionEvent(openedEvent)
        }
        work = opened
        previewPager = PreviewPager(local: found.thumbnails ?? [], remote: site.thumbnails, total: found.thumbnailsTotal)
        previewError = nil
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

  private func remote(_ text: String) -> some View {
    RemoteText(original: text, dictionary: nil, session: session, library: library, locale: locale, page: translationPage)
  }

  private func morePreviews() async {
    previewBusy = true
    defer { previewBusy = false }
    previewError = nil
    if previewPager.appendLocal() { return }
    do {
      let batch = try await prompt.perform {
        try await session.siteThumbnails(library, site: site.key, work: workId, page: previewPager.nextRemotePage, confirmation: $0)
      }
      try Task.checkCancellation()
      previewPager.append(batch)
    } catch {
      if !Task.isCancelled { previewPager.stopAutomatic(); previewError = siteMessage(error) }
    }
  }
}

@MainActor private struct DetailTagRow: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let tag: DetailSearchTag
  let locale: String
  let localText: String?
  let tr: (String) -> String
  @ObservedObject var translationPage: TranslationPage
  let isAuthor: Bool
  let canOpen: Bool
  let open: () -> Void
  @State private var translating = false
  @State private var checkingSubscription = false
  @State private var checkFailed = false
  @State private var result: String?
  @State private var failed = false
  @State private var attempt = 0
  @State private var saving = false
  @State private var status: String?

  var body: some View {
    VStack(alignment: .leading) {
      HStack {
        let label = Group {
          if isAuthor { Text(tag.name) }
          else { RemoteText(original: tag.name, dictionary: localText, session: session, library: library, locale: locale, page: translationPage) }
        }
        if canOpen { Button(action: open) { label } }
        else { label }
        Menu { actions } label: {
          Image(systemName: "ellipsis.circle")
            .accessibilityLabel(tr("更多") + " · " + tag.name)
        }.fixedSize()
      }.contextMenu { actions }.disabled(saving)
      if saving { ProgressView() }
      if let status { Text(status).font(.caption) }
      if checkFailed {
        Text(tr("部分订阅检查失败，请查看订阅状态")).font(.caption)
        Button(tr("重试")) { checkSubscription() }
      }
    }.sheet(isPresented: $translating) {
      VStack(alignment: .leading, spacing: 12) {
        Text(tr("划词翻译")).font(.headline)
        Text(tag.name).textSelection(.enabled)
        if localText != nil { Text(tr("词库译文")).font(.caption) }
        if let text = localText ?? result {
          Text(text).textSelection(.enabled)
          Button(tr("复制译文")) { copy(text) }
        } else if failed {
          Text(tr("翻译失败，请重试"))
          Button(tr("重试")) { attempt += 1 }
        } else { ProgressView() }
        Button(tr("关闭")) { translating = false }.keyboardShortcut(.cancelAction)
      }.padding().task(id: attempt) {
        guard localText == nil else { return }
        failed = false
        do {
          let text = try await session.translateTag(tag.name, locale: locale, library: library)
          try Task.checkCancellation()
          result = text
        } catch { if !Task.isCancelled { failed = true } }
      }
    }
  }

  @ViewBuilder private var actions: some View {
    Button(tr("复制")) {
      copy(tag.name)
      status = tr("已复制「{label}」").replacingOccurrences(of: "{label}", with: tag.name)
    }
    Button(tr("翻译")) { result = nil; failed = false; translating = true }
    if site.subscriptionUpdates {
      Button(tr("订阅此标签更新")) {
        save {
          try await session.subscribeToTag(tag, source: site.key, library: library)
          status = tr("已添加更新订阅")
          checkSubscription()
        }
      }
    }
    blockButton(tag.name, prompt: "屏蔽「{label}」")
    if !tag.namespace.isEmpty {
      blockButton(tag.namespace + ":" + tag.name, prompt: "仅屏蔽「{label}」")
    }
  }

  private func blockButton(_ entry: String, prompt: String) -> some View {
    Button(tr(prompt).replacingOccurrences(of: "{label}", with: entry)) {
      save {
        try await session.blockTag(entry)
        status = tr("已屏蔽「{label}」，列表中将隐藏带有该标签的作品").replacingOccurrences(of: "{label}", with: entry)
      }
    }
  }

  private func checkSubscription() {
    guard !checkingSubscription else { return }
    checkingSubscription = true
    checkFailed = false
    Task {
      defer { checkingSubscription = false }
      do { checkFailed = try await !session.checkSubscriptions(library, force: true) }
      catch { if !Task.isCancelled { checkFailed = true } }
    }
  }

  private func save(_ operation: @escaping @MainActor () async throws -> Void) {
    saving = true
    status = nil
    Task {
      defer { saving = false }
      do { try await operation() }
      catch { if !Task.isCancelled { status = tr("保存失败，请重试") } }
    }
  }

  private func copy(_ value: String) {
    #if os(macOS)
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(value, forType: .string)
    #else
    UIPasteboard.general.string = value
    #endif
  }
}

@MainActor private struct DetailThumbnail: View {
  let session: ProgressSession
  let library: SourceLibrary
  let source: String
  let item: SiteThumbnail
  @State private var image: CGImage?

  var body: some View {
    Group {
      if let image { Image(decorative: image, scale: 1).resizable().scaledToFit() }
      else { Image(systemName: "photo").foregroundStyle(.secondary) }
    }.frame(height: 90).accessibilityHidden(true)
    .task(id: item) {
      guard image == nil else { return }
      do {
        let asset = try await session.siteImage(library, site: source, address: item.url)
        let result: CGImage
        if let width = item.width, let height = item.height, width > 0, height > 0,
          item.offsetX != nil || item.offsetY != nil {
          result = try await Task.detached {
            try withExtendedLifetime(asset) {
              let url = URL(fileURLWithPath: try asset.path())
              let box = CGRect(x: item.offsetX ?? 0, y: item.offsetY ?? 0, width: width, height: height)
              guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
                    let sprite = CGImageSourceCreateImageAtIndex(source, 0, nil),
                    let tile = sprite.cropping(to: box) else { throw CocoaError(.fileReadCorruptFile) }
              return tile
            }
          }.value
        } else { result = try await decodedPage(asset, maxSize: 320) }
        try Task.checkCancellation()
        image = result
      } catch { }
    }
  }
}

@MainActor private struct ContentLinkDestination: View {
  @Environment(\.openURL) private var openURL
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let address: String
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let close: () -> Void
  @State private var target: SiteLinkTarget?
  @State private var problem: String?
  @State private var attempt = 0
  @StateObject private var prompt = SitePrompt()

  var body: some View {
    Group {
      switch target {
      case .work(let source, let id):
        SiteDetails(session: session, library: library, site: source, workId: id, preview: nil,
          locale: locale, incognito: incognito, tr: tr, close: close)
      case .search(let source, let url):
        SourceResults(session: session, library: library, site: source, locale: locale,
          incognito: incognito, tr: tr, startingLink: url, close: close)
      default:
        VStack {
          Button(tr("返回"), action: close)
          if let problem {
            Text(tr(problem))
            Button(tr("重试")) { attempt += 1 }
          } else { ProgressView() }
        }
      }
    }.task(id: attempt) {
      problem = nil
      do {
        let result = try await prompt.perform {
          try await session.contentLink(library, source: site, address: address, confirmation: $0)
        }
        try Task.checkCancellation()
        if case .external(let url) = result {
          openURL(url) { opened in if opened { close() } else { problem = "无法打开原站" } }
        } else { target = result }
      } catch { if !Task.isCancelled { problem = siteMessage(error) } }
    }.siteConfirmation(prompt, tr: tr)
  }
}

@MainActor private struct SiteCommentBody: View {
  let row: SiteComment
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  @State private var revealed = false
  @State private var expanded = false
  @State private var link: String?

  private var long: Bool { row.body.reduce(0) { $0 + ($1.text?.count ?? 0) } > 800 }
  private var parts: [(index: Int, text: String)] {
    var remaining = 800
    return row.body.enumerated().compactMap { index, part in
      let text = part.text ?? ""
      if expanded { return (index,text) }
      guard remaining > 0 else { return nil }
      let preview = String(text.prefix(remaining))
      remaining -= preview.count
      return (index,preview)
    }
  }
  var body: some View {
    VStack(alignment: .leading) {
      if row.spoiler == true && !revealed {
        Button(tr("显示剧透评论")) { revealed = true }
      } else {
        ForEach(parts, id: \.index) { element in
          let part = row.body[element.index]
          if part.type == "image", let address = part.url {
            ArticlePicture(session: session, library: library, site: site.key, address: address,
              title: element.text, maximum: 1000, tr: tr, openFailedImage: { link = address })
          } else if part.type == "link", let address = part.url {
            Button(element.text.isEmpty ? address : element.text) { link = address }
          } else if !element.text.isEmpty { Text(element.text) }
        }
        if long { Button(tr(expanded ? "收起" : "展开全文")) { expanded.toggle() } }
      }
    }.sheet(isPresented: Binding(get: { link != nil },set: { if !$0 { link = nil } })) {
      if let address = link {
        ContentLinkDestination(session: session, library: library, site: site, address: address, locale: locale,
          incognito: incognito, tr: tr, close: { link = nil })
      }
    }
  }
}

@MainActor private struct DetailEntriesPanel: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: InstalledSite
  let work: SiteWork
  let kind: DetailSectionKind
  let locale: String
  let incognito: Bool
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
  @State private var refreshing = false
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
              refreshing = false
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
              SiteCommentBody(row: row, session: session, library: library, site: site, locale: locale, incognito: incognito, tr: tr)
              if row.repliesAvailable == true || row.replies?.isEmpty == false {
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
            if kind == .comments && (parent == nil || parent?.repliesAvailable == true) {
              Button(tr("刷新")) { refreshing = true; attempt += 1 }
            }
          }
        }.task(id: attempt) { await load() }
      }
    }.siteConfirmation(prompt, tr: tr)
    .sheet(item: $selectedReply) { row in
      VStack(alignment: .leading) {
        Button(tr("返回")) { selectedReply = nil }
        ScrollView {
          DetailEntriesPanel(session: session, library: library, site: site, work: work, kind: .comments,
            locale: locale, incognito: incognito, tr: tr, select: select, parent: row)
        }
      }.padding()
    }
  }

  private func load() async {
    guard delivered != attempt, let page = (refreshing ? 1 : nextPage) else { return }
    if let parent, delivered == nil && entries.isEmpty {
      entries = (parent.replies ?? []).map(DetailEntry.comment)
      if parent.repliesAvailable != true { nextPage = nil; delivered = attempt; return }
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
          cursor: refreshing ? nil : cursor, confirmation: consent, sort: sort, parent: parent?.id)
      }
      try Task.checkCancellation()
      if result.reset || refreshing { entries.removeAll() }
      if !result.orders.isEmpty { orders = result.orders }
      var seen = Set(entries.map(\.id))
      let fresh = result.entries.filter { seen.insert($0.id).inserted }
      entries.append(contentsOf: fresh)
      nextPage = page > 1 && fresh.isEmpty && !result.entries.isEmpty ? nil : result.nextPage
      cursor = result.cursor
      delivered = attempt
      refreshing = false
    } catch { if !Task.isCancelled { problem = siteMessage(error) } }
    if !Task.isCancelled { loading = false }
  }
}
