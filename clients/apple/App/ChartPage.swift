import ParkCore
import SwiftUI

@MainActor struct ChartPage: View {
  let session: ProgressSession
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @State private var charts: [SiteChart]?
  @State private var active: SiteChart?
  @State private var settings: ChartSettings?
  @State private var busy = false
  @State private var error: String?
  @State private var attempt = 0

  var body: some View {
    VStack(alignment: .leading) {
      HStack {
        Button(tr("返回"), action: back)
        Text(tr("排行榜")).font(.headline)
      }.padding(.horizontal)
      if let error {
        Text(tr(error)).padding(.horizontal)
        Button(tr("重试")) { attempt += 1 }.padding(.horizontal)
      } else if let charts, charts.isEmpty { Text(tr("暂无可用排行榜")).padding() }
      else if let charts, let active, let settings, let library = InstalledSources.library {
        ScrollView(.horizontal) {
          HStack {
            choice(categoryName(active.category), charts.map(\.category), categoryName) { select(settings.board(in: charts, category: $0)) }
            let rows = charts.filter { $0.category == active.category }
            choice(sectionName(active.section), rows.map(\.section), sectionName) { select(settings.board(in: charts, category: active.category, section: $0)) }
            let group = rows.filter { $0.section == active.section }
            choice(kindName(active.kind), group.map(\.kind), kindName) { select(settings.board(in: charts, category: active.category, section: active.section, kind: $0)) }
            Menu(boardLabel(active)) {
              ForEach(group.filter { $0.kind == active.kind }) { board in
                Button(boardLabel(board)) { select(board) }
              }
            }
          }.disabled(busy).padding(.horizontal)
        }
        ChartResults(session: session, library: library, chart: active, charts: charts,
                     locale: locale, incognito: incognito, tr: tr, settings: settings) { self.settings = $0 }
          .id(active.id)
      } else { ProgressView().padding() }
    }.task(id: attempt) {
      do {
        guard let library = InstalledSources.library else { throw SiteReadError.malformed }
        let boards = try await session.charts(library, locale: locale)
        let saved = try await session.chartSettings()
        try Task.checkCancellation()
        charts = boards; settings = saved; active = saved.board(in: boards); error = nil
      } catch { if !Task.isCancelled { self.error = siteMessage(error) } }
    }
  }

  private func select(_ board: SiteChart?) {
    guard let board, !busy else { return }
    busy = true
    Task {
      defer { busy = false }
      do { settings = try await session.rememberChart(board); active = board }
      catch { self.error = "保存失败，请重试" }
    }
  }
  private func boardLabel(_ board: SiteChart) -> String { (board.site.name ?? board.site.key) + " · " + board.label }
  private func choice(_ title: String, _ keys: [String], _ label: @escaping (String) -> String,
                      _ change: @escaping (String) -> Void) -> some View {
    var seen = Set<String>()
    let values = keys.filter { seen.insert($0).inserted }
    return Menu(title) { ForEach(values, id: \.self) { key in Button(label(key)) { change(key) } } }
  }
  private func sectionName(_ id: String) -> String { tr(id == "market" ? "市场排行榜" : "站点排行榜") }
  private func kindName(_ id: String) -> String { tr(["work":"作品榜","seller":"卖家榜","actress":"女优榜","genre":"分类榜"][id] ?? id) }
  private func categoryName(_ id: String) -> String {
    tr(["feed":"吃瓜","gallery":"三次元美图","illustration":"二次元美图","doujin":"本子","anime":"里番",
        "jav":"JAV","adult":"成人视频","shortvideo":"短视频","aiDrama":"AI短剧","recording":"录播",
        "live":"直播","asmr":"音声","novel":"小说","tavern":"酒馆","onlineGame":"在线游戏","game":"游戏资源","other":"创作社区"][id] ?? id)
  }
}

@MainActor private struct ChartResults: View {
  let session: ProgressSession
  let library: SourceLibrary
  let chart: SiteChart
  let charts: [SiteChart]
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let settings: ChartSettings
  let saved: (ChartSettings) -> Void
  @StateObject private var prompt = SitePrompt()
  @State private var definitions: [PluginOption]?
  @State private var values: [String: PluginOptionValue] = [:]
  @State private var rows = [SiteWork]()
  @State private var candidates = [InstalledSite]()
  @State private var searchSites = [InstalledSite]()
  @State private var cursor: String?
  @State private var offset = 0
  @State private var page = 1
  @State private var more = true
  @State private var loading = true
  @State private var error: String?
  @State private var revision = 0
  @State private var original: URL?
  @State private var originalReady = false
  @State private var originalFailed = false
  @State private var search: String?
  @State private var work: String?
  @State private var info: SiteWork?
  @State private var filters = false
  @State private var configuring = false
  @State private var committing = false

  var body: some View {
    Group {
      if let original {
        VStack {
          Button(tr("返回")) { self.original = nil }
          IsolatedSitePage(policy: .confirmation(original), originalSite: incognito ? nil : chart.site.key,
                           ready: $originalReady, failed: $originalFailed)
          if originalFailed { Text(tr("站点加载失败，请重试")) }
          else if !originalReady { ProgressView() }
        }
      } else if let search {
        AggregateSearchPage(session: session, locale: locale, incognito: incognito, tr: tr, back: { self.search = nil },
                            initialKeyword: search, category: chart.category, preferred: settings.searchSources?[chart.category])
      } else if let work {
        SiteDetails(session: session, library: library, site: chart.site, workId: work, preview: nil,
                    locale: locale, incognito: incognito, tr: tr) { self.work = nil }
      } else if let info {
        ChartInformation(session: session, library: library, chart: chart, row: info, sites: candidates,
                         preferred: settings.detailSources?[chart.category], locale: locale,
                         incognito: incognito, tr: tr) { self.info = nil }
      } else {
        List {
          if !chart.metric.isEmpty { Text(chart.metric) }
          if let definitions, !definitions.isEmpty { Button(tr("筛选")) { filters = true } }
          Button(tr("排行榜配置")) { configuring = true }
          ForEach(rows) { row in
            VStack(alignment: .leading, spacing: 6) {
              HStack {
                if let rank = row.rankingPosition { Text(String(rank)).monospacedDigit() }
                if let own = chart.workID(for: row) { Button(row.title) { work = own } }
                else if chart.kind == "seller" { Text(row.title) }
                else { Button(row.title) { search = row.rankingKeyword } }
              }
              if chart.kind == "work" && !candidates.isEmpty { Button(tr("详情")) { info = row }.buttonStyle(.borderless) }
              if let url = row.rankingLink {
                if chart.owns(url) {
                  Button(tr("打开原站")) { originalReady = false; originalFailed = false; original = url }.buttonStyle(.borderless)
                } else { Link(tr("打开原站"), destination: url).buttonStyle(.borderless) }
              }
            }
          }
          if let error { Text(tr(error)); Button(tr("重试")) { revision += 1 } }
          else if loading { ProgressView() }
          else if more { Button(tr("加载更多")) { revision += 1 } }
          else if rows.isEmpty { Text(tr("暂无内容")) }
        }
      }
    }.task(id: revision) {
      loading = true; error = nil
      defer { if !Task.isCancelled { loading = false } }
      do {
        if definitions == nil {
          candidates = try await session.chartDetailSites(library, chart: chart, charts: charts)
          searchSites = try await session.searchScope(library, category: chart.category).sites
          let defs = try await prompt.perform { try await session.chartOptions(library, chart: chart, confirmation: $0) }
          let resolved = settings.values(for: chart, definitions: defs)
          if settings.selected != chart.id || settings.options?[chart.id] != resolved {
            saved(try await session.rememberChart(chart, values: resolved))
          }
          try Task.checkCancellation()
          definitions = defs; values = resolved
        }
        let result = try await prompt.perform {
          try await session.chartPage(library, chart: chart, page: page, offset: offset, cursor: cursor, values: values, confirmation: $0)
        }
        try Task.checkCancellation()
        var ids = Set(rows.map(\.id))
        rows.append(contentsOf: result.items.filter { ids.insert($0.id).inserted })
        cursor = result.cursor; more = result.hasNext; offset += result.receivedCount; page += 1
      } catch { if !Task.isCancelled { self.error = siteMessage(error) } }
    }.siteConfirmation(prompt, tr: tr)
    .sheet(isPresented: $filters) {
      PluginFilterForm(localize: chart.localized, definitions: definitions ?? [], values: values, tr: tr) { next, _ in
        let state = try await session.rememberChart(chart, values: next)
        saved(state); values = next; page = 1; offset = 0; cursor = nil; rows = []; more = true; revision += 1
      }
    }
    .sheet(isPresented: $configuring) {
      VStack(alignment: .leading) {
        Text(tr("排行榜配置")).font(.headline)
        Menu(tr("详情站点")) {
          Button(tr("源站优先")) { configure(detail: "") }
          ForEach(candidates) { site in Button(site.name ?? site.key) { configure(detail: site.key) } }
        }
        Text(candidates.first { $0.key == settings.detailSources?[chart.category] }?.name ?? tr("源站优先"))
        Menu(tr("搜索站点")) {
          Button(tr("全部")) { configure(search: "") }
          Button(tr("域首选项")) { configure(search: "preferred") }
          ForEach(searchSites) { site in Button(site.name ?? site.key) { configure(search: site.key) } }
        }
        let chosen = settings.searchSources?[chart.category]
        Text(searchSites.first { $0.key == chosen }?.name ?? tr(chosen == "preferred" ? "域首选项" : "全部"))
        if let error { Text(tr(error)) }
        Button(tr("返回")) { configuring = false }
      }.padding().disabled(committing)
    }
  }

  private func configure(search: String? = nil, detail: String? = nil) {
    guard !committing else { return }
    committing = true
    Task {
      defer { committing = false }
      do {
        saved(try await session.rememberChart(chart, searchSite: search, detailSite: detail))
        configuring = false
      } catch { self.error = "保存失败，请重试" }
    }
  }
}

@MainActor struct PluginFilterForm: View {
  let localize: (String) -> String
  let definitions: [PluginOption]
  @State var values: [String: PluginOptionValue]
  let tr: (String) -> String
  var query: ((PluginOption, [String: PluginOptionValue]) -> SiteFilterQuery?)? = nil
  var load: ((SiteFilterQuery) async throws -> [PluginOption.Choice])? = nil
  var commonRules: [String: SiteCommonRule] = [:]
  var sharedDimensions: Set<String> = []
  @State var limits: [String: [String: String]] = [:]
  let apply: ([String: PluginOptionValue], [String: [String: String]]) async throws -> Void
  @Environment(\.dismiss) private var dismiss
  @State private var active: String?
  @State private var choices: [SiteFilterQuery: [PluginOption.Choice]] = [:]
  @State private var expiry: [SiteFilterQuery: Date] = [:]
  @State private var busy = false
  @State private var failed = false

  private func change(_ option: PluginOption, _ value: PluginOptionValue) {
    guard values[option.key] != value else { return }
    SiteBrowseOptions.reset([option.key], in: definitions, values: &values)
    values[option.key] = value
  }

  private func pick(_ option: PluginOption, _ choice: PluginOption.Choice, remote: Bool) -> some View {
    let selected = values[option.key]
    let members: [PluginOptionValue] = { if case .list(let group) = selected { return group }; return [] }()
    return Button {
      if option.type == "multi" {
        change(option, .list(members.contains(choice.value) ? members.filter { $0 != choice.value } : members + [choice.value]))
      } else { change(option, choice.value) }
    } label: {
      HStack {
        Text(remote ? choice.label : localize(choice.label)); Spacer()
        if selected == choice.value || members.contains(choice.value) { Image(systemName: "checkmark") }
      }
    }
  }

  @ViewBuilder private func commonFields(_ dimensions: [String]) -> some View {
    let labels = ["pages": "页数", "likes": "点赞数", "duration": "时长", "score": "评分", "favorites": "收藏数"]
    ForEach(dimensions, id: \.self) { dimension in
      if let rule = commonRules[dimension], let label = labels[dimension] {
        VStack(alignment: .leading) {
          Text(tr(label)).font(.headline)
          ForEach(["min", "max"].filter(rule.range.contains), id: \.self) { key in
            TextField(tr(key == "min" ? "最小值" : "最大值"), text: Binding(
              get: { limits[dimension]?[key] ?? "" },
              set: { limits[dimension, default: [:]][key] = $0 }))
          }
          if !rule.sort.isEmpty {
            Picker(tr("排序"), selection: Binding(get: { limits[dimension]?["order"] ?? "" }, set: { order in
              if !order.isEmpty {
                for other in SiteCommonRule.dimensions where other != dimension { limits[other]?["order"] = nil }
              }
              limits[dimension, default: [:]]["order"] = order
            })) {
              Text(tr("不限")).tag("")
              if rule.sort.contains("asc") { Text(tr("升序")).tag("asc") }
              if rule.sort.contains("desc") { Text(tr("降序")).tag("desc") }
            }
          }
        }
      }
    }
  }

  var body: some View {
    VStack {
      List {
        ForEach(definitions) { option in
          let label = localize(option.label ?? option.key)
          Section(header: Text(label)) {
            if option.dynamic == true, let query, let load {
              let request = query(option, values)
              Button(label) { active = active == option.key ? nil : option.key }.disabled(request == nil)
              if active == option.key, let request {
                RemoteFilterChoices(load: {
                  if let cached = choices[request], let end = expiry[request], end > Date() { return cached }
                  let rows = try await load(request)
                  try Task.checkCancellation()
                  choices[request] = rows
                  expiry[request] = Date().addingTimeInterval(120)
                  return rows
                }, tr: tr) { row in pick(option, row, remote: true) }.id(request)
              }
            } else if let options = option.values, !options.isEmpty {
              ForEach(options, id: \.value) { pick(option, $0, remote: false) }
            } else if option.type == "switch" {
              Toggle(label, isOn: Binding(get: { values[option.key] == .flag(true) }, set: { change(option, .flag($0)) }))
            } else if option.type != "select" && option.type != "multi" {
              let text = Binding<String>(get: {
                switch values[option.key] {
                case .text(let s): return s
                case .number(let n): return NSDecimalNumber(decimal: n).stringValue
                default: return ""
                }
              }, set: { change(option, .text($0)) })
              if option.type == "password" || option.secret == true { SecureField(label, text: text) }
              else { TextField(label, text: text) }
            }
          }
        }
        let dimensions = SiteCommonRule.dimensions.filter { commonRules[$0] != nil }
        commonFields(dimensions.filter { !sharedDimensions.contains($0) })
        let shared = dimensions.filter(sharedDimensions.contains)
        if !shared.isEmpty { Section(header: Text(tr("通用筛选"))) { commonFields(shared) } }
      }
      if failed { Text(tr("保存失败，请重试")) }
      HStack {
        Button(tr("取消")) { dismiss() }
        Button(tr("应用")) {
          busy = true
          Task {
            defer { busy = false }
            do {
              var next = values
              for option in definitions where option.type == "number" {
                if case .text(let input) = next[option.key] {
                  if input.isEmpty { next[option.key] = option.defaultValue; continue }
                  let n = try SiteCommonRule.number(input)
                  guard option.min.map({ n >= $0 }) ?? true,
                        option.max.map({ n <= $0 }) ?? true else { throw SiteReadError.malformed }
                  next[option.key] = .number(n)
                }
              }
              _ = try SiteCommonRule.validated(limits, rules: commonRules)
              try await apply(next, limits); dismiss()
            } catch { failed = true }
          }
        }
      }
    }.padding().disabled(busy).interactiveDismissDisabled(busy)
  }
}

@MainActor private struct RemoteFilterChoices<Choice: View>: View {
  let load: () async throws -> [PluginOption.Choice]
  let tr: (String) -> String
  @ViewBuilder let choice: (PluginOption.Choice) -> Choice
  @State private var rows: [PluginOption.Choice]?
  @State private var failed = false
  @State private var attempt = 0

  var body: some View {
    Group {
      if let rows {
        if rows.isEmpty { Text(tr("暂无内容")) }
        ForEach(rows, id: \.value) { choice($0) }
      } else if failed { Button(tr("重试")) { attempt += 1 } }
      else { ProgressView() }
    }.task(id: attempt) {
      failed = false
      do {
        let found = try await load()
        try Task.checkCancellation()
        rows = found
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}

@MainActor private struct ChartInformation: View {
  let session: ProgressSession
  let library: SourceLibrary
  let chart: SiteChart
  let row: SiteWork
  let sites: [InstalledSite]
  let preferred: String?
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  @StateObject private var prompt = SitePrompt()
  @State private var choice: String?
  @State private var references = [ChartReference]()
  @State private var works = [SiteWork]()
  @State private var detail: ChartReference?
  @State private var work: String?
  @State private var selectedID: String?
  @State private var automatic = false
  @State private var loading = true
  @State private var error: String?
  @State private var attempt = 0

  private var ordered: [InstalledSite] {
    sites.filter { $0.key == chart.site.key } + sites.filter { $0.key != chart.site.key }
  }
  private var site: InstalledSite? {
    ordered.first { $0.key == choice } ?? ordered.first { $0.key == preferred } ?? ordered.first
  }
  private func closeDetail() {
    if automatic { back() }
    else { selectedID = nil; detail = nil; work = nil }
  }

  var body: some View {
    Group {
      if let work, let site {
        SiteDetails(session: session, library: library, site: site, workId: work,
                    preview: works.first { $0.id == work }, locale: locale, incognito: incognito, tr: tr, close: closeDetail)
      } else {
        List {
          Button(tr("返回"), action: detail != nil || selectedID != nil ? closeDetail : back)
          if let detail {
            Text(detail.title).font(.headline)
            if let code = detail.code { Text(code) }
            if let description = detail.description { Text(description) }
          } else {
            Text(row.title).font(.headline)
            if let error { Text(tr(error)); Button(tr("重试")) { attempt += 1 } }
            else if loading { ProgressView() }
            else if references.isEmpty && works.isEmpty { Text(tr("未找到作品资料")) }
            else {
              Text(tr("选择作品"))
              ForEach(references) { item in Button(item.title) { automatic = false; selectedID = item.id; attempt += 1 } }
              ForEach(works) { item in Button(item.title) { automatic = false; work = item.id } }
            }
            if preferred.flatMap({ key in sites.first { $0.key == key } }) == nil {
              ForEach(ordered.filter { $0.key != site?.key }) { other in Button(other.name ?? other.key) { switchSite(other) } }
            }
          }
        }
      }
    }.task(id: attempt) {
      guard let site else { loading = false; return }
      loading = true; error = nil
      defer { if !Task.isCancelled { loading = false } }
      do {
        if site.key == chart.site.key, let own = chart.workID(for: row) { automatic = true; work = own; return }
        if site.metadataDetail && (site.metadataSearch || site.key == chart.site.key) {
          if selectedID == nil && site.key == chart.site.key { selectedID = row.id; automatic = true }
          if selectedID == nil {
            references = try await prompt.perform { try await session.chartReferences(library, site: site, chart: chart, row: row, confirmation: $0) }
            let exact = references.filter { $0.matches(row) }
            if exact.count == 1 { selectedID = exact[0].id; automatic = true }
          }
          if let selectedID {
            detail = try await prompt.perform { try await session.chartReference(library, site: site, chart: chart, row: row, referenceID: selectedID, confirmation: $0) }
          }
        } else if site.search == true && site.detail {
          works = try await prompt.perform {
            try await session.sitePage(library, site: site, keyword: row.rankingKeyword, page: 1, cursor: nil, confirmation: $0).items
          }
          let exact = works.filter { $0.matchesChartWork(row) }
          if exact.count == 1 { automatic = true; work = exact[0].id }
        }
        try Task.checkCancellation()
        if works.isEmpty && references.isEmpty && detail == nil && work == nil,
           !ordered.contains(where: { $0.key == preferred }), let index = ordered.firstIndex(where: { $0.key == site.key }),
           index + 1 < ordered.count { switchSite(ordered[index + 1]) }
      } catch { if !Task.isCancelled { self.error = siteMessage(error) } }
    }.siteConfirmation(prompt, tr: tr)
  }

  private func switchSite(_ next: InstalledSite) {
    choice = next.key; works = []; references = []; detail = nil; work = nil; selectedID = nil; attempt += 1
  }
}
