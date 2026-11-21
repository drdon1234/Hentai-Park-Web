import ParkCore
import SwiftUI

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
  let session: ProgressSession
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let close: () -> Void
  @State private var sites: [InstalledSite]?
  @State private var chosen: InstalledSite?
  @State private var failed = false
  @State private var retry = 0

  var body: some View {
    if let chosen, let library = InstalledSources.library {
      SourceResults(
        session: session, library: library, site: chosen, locale: locale, incognito: incognito,
        tr: tr
      ) { self.chosen = nil }.id(chosen.id)
    } else {
      VStack(alignment: .leading) {
        HStack {
          Button(tr("返回"), action: close)
          Text(tr("全部站点")).font(.headline)
        }.padding()
        if failed {
          Button(tr("站点加载失败，请重试")) { retry += 1 }.padding()
        } else if let sites {
          if sites.isEmpty { Text(tr("暂无内容")).padding() }
          List(sites) { site in
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

@MainActor private struct SourceResults: View {
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
  @State private var article: [SiteArticleBlock] = []
  @State private var selectedBlock: Int?
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
            } else if imageFailed {
              Button(tr("重试")) { imageAttempt += 1 }
            }
            if let description = detail.description { Text(description) }
            if !article.isEmpty {
              ArticleBody(session: session, library: library, site: site.key, title: detail.title,
                blocks: article, tr: tr, select: { selectedBlock = $0 })
            }
            if let work {
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
    }.task(id: attempt) {
      guard detail == nil else { return }
      problem = nil
      do {
        let found = try await prompt.perform {
          try await session.siteDetail(library, site: site.key, id: workId, preview: preview, confirmation: $0)
        }
        let opened = try await session.openSiteWork(
          library, site: site, item: found, incognito: incognito)
        if opened.media == "article", opened.contentDomain == "feed" { article = try found.articleBlocks() }
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
