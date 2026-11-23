import ParkCore
import SwiftUI

private extension View {
    @ViewBuilder func shelfSheetSize() -> some View {
        #if os(macOS)
        self.frame(minWidth: 360, minHeight: 300)
        #else
        self
        #endif
    }
}

@MainActor private struct ShelfMigration: View {
    let work: LibraryWork
    let session: ProgressSession
    let incognito: Bool
    let tr: (String) -> String
    let cancel: () -> Void
    let completed: (String) -> Void
    @StateObject private var prompt = SitePrompt()
    @State private var sites: [InstalledSite]?
    @State private var site: InstalledSite?
    @State private var results: [SiteWork]?
    @State private var choice: SiteWork?
    @State private var term = ""
    @State private var categories = true
    @State private var progress = true
    @State private var busy = false
    @State private var failed = false
    @State private var request = 0

    var body: some View {
        VStack(alignment: .leading) {
            Text(tr("迁移到其他站点")).font(.headline)
            Text(work.title)
            if let site, let choice {
                Text(site.name ?? site.key)
                Text(choice.title)
                Toggle(tr("复制分类"), isOn: $categories)
                if work.media != "game" {
                    Toggle(tr("复制已读章节"), isOn: $progress).disabled(incognito)
                }
                HStack {
                    Button(tr("复制")) { apply(site, choice, remove: false) }
                    Button(tr("迁移")) { apply(site, choice, remove: true) }
                }
                Button(tr("返回")) { self.choice = nil; failed = false }
            } else if let site {
                Text(site.name ?? site.key)
                TextField(tr("搜索"), text: $term)
                Button(tr("搜索")) { request += 1 }.disabled(term.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                if let results {
                    if results.isEmpty { Text(tr("没有找到")) }
                    List(results) { row in Button(row.title) { choice = row; failed = false } }
                }
                Button(tr("返回")) { self.site = nil; results = nil; failed = false }
            } else if let sites {
                if sites.isEmpty { Text(tr("没有找到")) }
                List(sites) { target in Button(target.name ?? target.key) {
                    site = target; term = work.title; request += 1
                } }
            }
            if busy { ProgressView() }
            if failed {
                Text(tr("加载失败"))
                if choice == nil { Button(tr("重试")) { request += 1 } }
            }
            Button(tr("取消"), action: cancel)
        }
        .padding().shelfSheetSize().disabled(busy).interactiveDismissDisabled(busy)
        .siteConfirmation(prompt, tr: tr)
        .task(id: request) {
            busy = true; failed = false
            defer { if !Task.isCancelled { busy = false } }
            do {
                guard let library = InstalledSources.library else { throw SiteReadError.malformed }
                if let site {
                    results = nil
                    let page = try await prompt.perform { confirmation in
                        try await session.sitePage(library, site: site, keyword: term.trimmingCharacters(in: .whitespacesAndNewlines),
                            page: 1, cursor: nil, confirmation: confirmation)
                    }
                    try Task.checkCancellation()
                    results = page.items
                } else {
                    sites = try await session.sites(library).filter {
                        $0.key != work.source && $0.mediaType == work.media && $0.search == true && $0.detail
                    }
                }
            } catch { if !Task.isCancelled { failed = true } }
        }.onAppear { progress = !incognito && work.media != "game" }
    }

    private func apply(_ site: InstalledSite, _ choice: SiteWork, remove: Bool) {
        guard !busy, let library = InstalledSources.library else { return }
        busy = true; failed = false
        Task {
            defer { busy = false }
            do {
                let detail = try await prompt.perform { confirmation in
                    try await session.siteDetail(library, site: site.key, id: choice.id,
                        preview: choice, confirmation: confirmation)
                }
                let removed = try await session.migrateFavorite(from: work, to: site, item: detail,
                    categories: categories, progress: progress && !incognito && work.media != "game", remove: remove)
                if removed {
                    completed(tr("已迁移到 {value0}").replacingOccurrences(of: "{value0}", with: site.name ?? site.key))
                } else {
                    completed(tr(remove ? "已复制到新的站点；未匹配的进度保留在原收藏中" : "已复制到新的站点"))
                }
            } catch { failed = true }
        }
    }
}

@MainActor final class ShelfOptions: ObservableObject {
    @Published var term = ""
    @Published var category = ""
    @Published var display = ShelfPreferences()
    @Published var stats: [String: FavoriteCardInfo] = [:]
    @Published var categories: [LibraryCategory] = []
    @Published var selected = Set<LibraryWorkID>()
    @Published var selecting = false
    @Published var failed = false

    func refresh(_ session: ProgressSession) async {
        do {
            let info = try await session.favoriteStats()
            let groups = try await session.shelfCategories()
            let prefs = try await session.shelfPreferences()
            try Task.checkCancellation()
            stats = info
            categories = groups
            display = prefs
            if !groups.contains(where: { $0.name == category }) { category = "" }
            failed = false
        } catch { if !Task.isCancelled { failed = true } }
    }

    func rows(_ works: [LibraryWork]) -> [LibraryWork] {
        let sort = display.librarySort, ascending = display.librarySortAsc
        let sources = Set(display.libraryFilterSources)
        func info(_ work: LibraryWork) -> FavoriteCardInfo? { stats[work.source + ":" + work.workId] }
        func rank(_ work: LibraryWork) -> Int64 {
            switch sort {
            case "read": return work.historyMs ?? 0
            case "updated": return info(work)?.updated ?? 0
            default: return info(work)?.added ?? 0
            }
        }
        return works.filter { work in
            work.favorite && (term.isEmpty || work.title.localizedCaseInsensitiveContains(term)) &&
                (category.isEmpty || work.categoryNames.contains(category)) &&
                (display.libraryFilterKind == "all" || work.media == display.libraryFilterKind) &&
                (sources.isEmpty || sources.contains(work.source)) &&
                (!display.libraryFilterUnread || (info(work)?.unread ?? 0) > 0) &&
                (!display.libraryFilterDownloaded || info(work)?.downloaded == true)
        }.sorted { a, b in
            let titles = a.title.localizedStandardCompare(b.title)
            if sort == "title", titles != .orderedSame { return ascending ? titles == .orderedAscending : titles == .orderedDescending }
            if sort != "title", rank(a) != rank(b) { return ascending ? rank(a) < rank(b) : rank(a) > rank(b) }
            if titles != .orderedSame { return titles == .orderedAscending }
            return (a.source, a.workId) < (b.source, b.workId)
        }
    }

    func finishSelection() {
        selecting = false
        selected.removeAll()
    }
}

@MainActor struct ShelfWorkGrid: View {
    @ObservedObject var options: ShelfOptions
    let works: [LibraryWork]
    let session: ProgressSession
    let tr: (String) -> String
    let details: (LibraryWork) -> Void
    let open: (LibraryWork, String) -> Void
    @State private var expanded: LibraryWorkID?

    var body: some View {
        let textOnly = works.allSatisfy { $0.media == "novel" && (options.stats[$0.source + ":" + $0.workId]?.cover ?? "").isEmpty }
        let count = options.display.gridColumns
        let columns = textOnly ? [GridItem(.flexible())] : count == 0
            ? [GridItem(.adaptive(minimum: 128))]
            : Array(repeating: GridItem(.flexible(minimum: 30), alignment: .topLeading), count: count)
        LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
            ForEach(works, id: \.libraryID) { work in
                let info = options.stats[work.source + ":" + work.workId]
                let selected = options.selected.contains(work.libraryID)
                VStack(alignment: .leading, spacing: 4) {
                    Button {
                        if !options.selecting { details(work) }
                        else if selected { options.selected.remove(work.libraryID) }
                        else { options.selected.insert(work.libraryID) }
                    } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            if let cover = info?.cover, !cover.isEmpty {
                                ShelfCover(session: session, source: work.source, address: cover)
                            }
                            if options.selecting {
                                Label(work.title, systemImage: selected ? "checkmark.circle.fill" : "circle")
                            } else { Text(work.title) }
                        }.frame(maxWidth: .infinity, alignment: .leading).contentShape(Rectangle())
                    }.buttonStyle(.borderless).accessibilityLabel(Text(work.title))
                        .accessibilityAddTraits(selected ? [.isSelected] : [])
                    if options.display.badgeSource { Text(work.source).font(.caption) }
                    if options.display.badgeUnread, let info, !info.oneShot, info.unread > 0 {
                        Text(tr("未读") + " " + String(info.unread)).font(.caption)
                    }
                    if options.display.badgeDownloaded, info?.downloaded == true { Text(tr("已下载")).font(.caption) }
                    if !options.selecting, work.downloaded, !work.invalidDownload, !work.chapters.isEmpty {
                        Button(tr(expanded == work.libraryID ? "收起" : "下载的章节")) {
                            expanded = expanded == work.libraryID ? nil : work.libraryID
                        }.buttonStyle(.borderless)
                        if expanded == work.libraryID {
                            ForEach(work.chapters, id: \.id) { chapter in
                                Button(chapter.title) { open(work, chapter.id) }.buttonStyle(.borderless)
                            }
                        }
                    }
                }.accessibilityElement(children: .contain)
            }
        }
    }
}

@MainActor private struct ShelfCover: View {
    let session: ProgressSession
    let source: String
    let address: String
    @Environment(\.blurCovers) private var blurCovers
    @State private var pixels: CGImage?

    var body: some View {
        Group {
            if let pixels {
                Image(decorative: pixels, scale: 1).resizable().scaledToFit().blur(radius: blurCovers ? 14 : 0)
            } else { Image(systemName: "book.closed").foregroundColor(.secondary) }
        }.frame(maxWidth: .infinity, maxHeight: 150).accessibilityHidden(true)
            .task(id: address) {
                pixels = nil
                guard let library = InstalledSources.library else { return }
                do {
                    let asset = try await session.siteImage(library, site: source, address: address)
                    let image = try await decodedPage(asset, maxSize: 360)
                    try Task.checkCancellation()
                    pixels = image
                } catch { }
            }
    }
}

@MainActor struct ShelfControls: View {
    private enum Panel: String, Identifiable {
        case filters, categories, migration
        var id: String { rawValue }
    }
    @ObservedObject var options: ShelfOptions
    let session: ProgressSession
    let visible: [LibraryWork]
    let sources: [String]
    let incognito: Bool
    let tr: (String) -> String
    let refresh: () -> Void
    @State private var panel: Panel?
    @State private var draft = ShelfPreferences()
    @State private var searchDraft = "", categoryDraft = ""
    @State private var removing = false
    @State private var working = false
    @State private var failed = false
    @State private var name = ""
    @State private var migration: [LibraryWork] = []
    @State private var outcome = ""

    private var targets: [LibraryWork] { visible.filter { options.selected.contains($0.libraryID) } }
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if options.failed {
                action("重试") { Task { await options.refresh(session) } }
            }
            if options.selecting {
                action("取消选择") { options.finishSelection() }
                action("分类") { name = ""; panel = .categories }.disabled(targets.isEmpty)
                action("迁移到其他站点") { migration = targets; panel = .migration }.disabled(targets.isEmpty)
                action("标为已读") { apply(.markRead) }.disabled(targets.isEmpty)
                action("取消收藏") { removing = true }.disabled(targets.isEmpty)
            } else {
                action("筛选与排序") {
                    draft = options.display; searchDraft = options.term; categoryDraft = options.category
                    failed = false; panel = .filters
                }
                action("批量选择") { options.selecting = true }
            }
            if failed { Text(tr("保存失败，请重试")).foregroundColor(.secondary) }
            if !outcome.isEmpty { Text(outcome) }
        }
        .disabled(working)
        .sheet(item: $panel, onDismiss: { migration = [] }) { sheet in
            switch sheet {
            case .filters: filterPanel
            case .categories: categoryPanel
            case .migration:
                if let work = migration.first {
                    ShelfMigration(work: work, session: session, incognito: incognito, tr: tr,
                        cancel: { panel = nil }, completed: { message in
                            outcome = message
                            migration.removeFirst()
                            if migration.isEmpty { options.finishSelection(); panel = nil }
                            refresh()
                        }).id(work.libraryID)
                }
            }
        }
        .alert(tr("取消收藏 {value0} 部作品？").replacingOccurrences(of: "{value0}", with: String(targets.count)),
               isPresented: $removing) {
            Button(tr("取消"), role: .cancel) {}
            Button(tr("取消收藏"), role: .destructive) { apply(.remove) }
        }
    }

    private func action(_ key: String, _ perform: @escaping () -> Void) -> some View {
        Button(action: perform) {
            Text(tr(key)).frame(maxWidth: .infinity, alignment: .leading)
                .padding(.vertical, 6).contentShape(Rectangle())
        }.buttonStyle(.borderless)
    }

    private var filterPanel: some View {
        VStack {
            Form {
                TextField(tr("搜索"), text: $searchDraft)
                Picker(tr("分类"), selection: $categoryDraft) {
                    Text(tr("全部分类")).tag("")
                    ForEach(options.categories, id: \.id) { Text($0.name).tag($0.name) }
                }
                Picker(tr("媒体类型"), selection: $draft.libraryFilterKind) {
                    ForEach(["all","comic","novel","video","voice","game","article"], id: \.self) { key in
                        Text(tr(["all":"全部","comic":"漫画","novel":"小说","video":"视频","voice":"音声","game":"游戏","article":"文章"][key] ?? key)).tag(key)
                    }
                }
                Section(header: Text(tr("站点"))) {
                    ForEach(Array(Set(sources + draft.libraryFilterSources)).sorted(), id: \.self) { source in
                        Toggle(source, isOn: Binding(get: { draft.libraryFilterSources.contains(source) }, set: { selected in
                            draft.libraryFilterSources.removeAll { $0 == source }
                            if selected { draft.libraryFilterSources.append(source) }
                        }))
                    }
                }
                Toggle(tr("有未读"), isOn: $draft.libraryFilterUnread)
                Toggle(tr("已下载"), isOn: $draft.libraryFilterDownloaded)
                Picker(tr("排序"), selection: $draft.librarySort) {
                    ForEach(["added","title","read","updated"], id: \.self) { key in
                        Text(tr(["added":"添加时间","title":"标题","read":"最近阅读","updated":"最近更新"][key] ?? "")).tag(key)
                    }
                }
                Toggle(tr("升序"), isOn: $draft.librarySortAsc)
                Picker(tr("每行封面数"), selection: $draft.gridColumns) {
                    Text(tr("自动")).tag(0)
                    ForEach([2,3,4], id: \.self) { Text(String($0)).tag($0) }
                }
                Toggle(tr("站点图标"), isOn: $draft.badgeSource)
                Toggle(tr("未读数徽标"), isOn: $draft.badgeUnread)
                Toggle(tr("已下载徽标"), isOn: $draft.badgeDownloaded)
            }
            if failed { Text(tr("保存失败，请重试")) }
            HStack {
                Button(tr("取消")) { panel = nil }
                Button(tr("完成")) {
                    working = true
                    Task {
                        defer { working = false }
                        do {
                            try await session.saveShelfOptions(draft)
                            options.display = draft; options.term = searchDraft; options.category = categoryDraft
                            failed = false; panel = nil
                        } catch { failed = true }
                    }
                }
            }
        }.padding().shelfSheetSize().disabled(working).interactiveDismissDisabled()
    }
    private var categoryPanel: some View {
        VStack {
            List(options.categories, id: \.id) { row in
                Button(row.name) { apply(.addCategory(id: row.id)) }
            }
            TextField(tr("新建分类"), text: $name)
            Button(tr("新建分类")) {
                working = true
                Task {
                    defer { working = false }
                    do { try await session.createShelfCategory(name); await options.refresh(session); name = ""; failed = false }
                    catch { failed = true }
                }
            }.disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            if failed { Text(tr("保存失败，请重试")) }
            Button(tr("取消")) { panel = nil }
        }.padding().shelfSheetSize().disabled(working)
    }

    private func apply(_ action: FavoriteEdit) {
        guard !working, !targets.isEmpty else { return }
        let selected = targets
        working = true
        Task {
            defer { working = false }
            do {
                try await session.editFavorites(selected, action: action)
                failed = false
                panel = nil
                options.finishSelection()
                refresh()
            } catch { failed = true }
        }
    }
}
