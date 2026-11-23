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
    @Published var unread = false
    @Published var downloaded = false
    @Published var sort = "added"
    @Published var ascending = false
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
            sort = prefs.librarySort ?? "added"
            ascending = prefs.librarySortAsc ?? false
            unread = prefs.libraryFilterUnread ?? false
            downloaded = prefs.libraryFilterDownloaded ?? false
            if !groups.contains(where: { $0.name == category }) { category = "" }
            failed = false
        } catch { if !Task.isCancelled { failed = true } }
    }

    func rows(_ works: [LibraryWork]) -> [LibraryWork] {
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
                (!unread || (info(work)?.unread ?? 0) > 0) && (!downloaded || info(work)?.downloaded == true)
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

@MainActor struct ShelfControls: View {
    @ObservedObject var options: ShelfOptions
    let session: ProgressSession
    let visible: [LibraryWork]
    let incognito: Bool
    let tr: (String) -> String
    let refresh: () -> Void
    @State private var filters = false
    @State private var classifying = false
    @State private var removing = false
    @State private var working = false
    @State private var failed = false
    @State private var name = ""
    @State private var migration: [LibraryWork] = []
    @State private var outcome = ""

    private var targets: [LibraryWork] { visible.filter { options.selected.contains($0.libraryID) } }
    var body: some View {
        Section {
            if options.failed {
                Button(tr("重试")) { Task { await options.refresh(session) } }
            }
            if options.selecting {
                Button(tr("取消选择")) { options.finishSelection() }
                Button(tr("分类")) { name = ""; classifying = true }.disabled(targets.isEmpty)
                Button(tr("迁移到其他站点")) { migration = targets }.disabled(targets.isEmpty)
                Button(tr("标为已读")) { apply(.markRead) }.disabled(targets.isEmpty)
                Button(tr("取消收藏")) { removing = true }.disabled(targets.isEmpty)
            } else {
                Button(tr("筛选与排序")) { filters = true }
                Button(tr("批量选择")) { options.selecting = true }
            }
            if failed { Text(tr("保存失败，请重试")).foregroundColor(.secondary) }
            if !outcome.isEmpty { Text(outcome) }
        }
        .disabled(working)
        .sheet(isPresented: Binding(get: { !migration.isEmpty }, set: { if !$0 { migration = [] } })) {
            if let work = migration.first {
                ShelfMigration(work: work, session: session, incognito: incognito, tr: tr,
                    cancel: { migration = [] }, completed: { message in
                        outcome = message
                        migration.removeFirst()
                        if migration.isEmpty { options.finishSelection() }
                        refresh()
                    }).id(work.libraryID)
            }
        }
        .sheet(isPresented: $filters) {
            VStack {
                Form {
                    TextField(tr("搜索"), text: $options.term)
                    Picker(tr("分类"), selection: $options.category) {
                        Text(tr("全部分类")).tag("")
                        ForEach(options.categories, id: \.id) { Text($0.name).tag($0.name) }
                    }
                    Toggle(tr("有未读"), isOn: $options.unread)
                    Toggle(tr("已下载"), isOn: $options.downloaded)
                    Picker(tr("排序"), selection: $options.sort) {
                        ForEach(["added","title","read","updated"], id: \.self) { key in
                            Text(tr(["added":"添加时间","title":"标题","read":"最近阅读","updated":"最近更新"][key] ?? "")).tag(key)
                        }
                    }
                    Toggle(tr("升序"), isOn: $options.ascending)
                }
                if failed { Text(tr("保存失败，请重试")) }
                Button(tr("完成")) {
                    working = true
                    Task {
                        defer { working = false }
                        do {
                            try await session.saveShelfOptions(sort: options.sort, ascending: options.ascending,
                                unread: options.unread, downloaded: options.downloaded)
                            failed = false; filters = false
                        } catch { failed = true }
                    }
                }
            }.padding().shelfSheetSize().disabled(working).interactiveDismissDisabled()
        }
        .sheet(isPresented: $classifying) {
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
                Button(tr("取消")) { classifying = false }
            }.padding().shelfSheetSize().disabled(working)
        }
        .alert(tr("取消收藏 {value0} 部作品？").replacingOccurrences(of: "{value0}", with: String(targets.count)),
               isPresented: $removing) {
            Button(tr("取消"), role: .cancel) {}
            Button(tr("取消收藏"), role: .destructive) { apply(.remove) }
        }
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
                classifying = false
                options.finishSelection()
                refresh()
            } catch { failed = true }
        }
    }
}
