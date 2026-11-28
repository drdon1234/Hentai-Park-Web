import ParkCore
import SwiftUI

@MainActor struct HistoryRowMenu: ViewModifier {
    let enabled: Bool
    let work: LibraryWork
    let session: ProgressSession?
    let offersInteractions: Bool
    let tr: (String) -> String
    let detail: () -> Void
    @State private var removing = false
    @State private var categories = false
    @State private var authors = false
    @State private var interactions = false
    @State private var failed = false
    @State private var busy = false
    @ViewBuilder func body(content: Content) -> some View {
        if enabled, let session {
            content.contextMenu {
                Button(tr("详情"), action: detail)
                if offersInteractions { Button(tr("原站互动")) { interactions = true } }
                Button(tr(work.favorite ? "取消应用内收藏" : "应用内收藏")) {
                    run { try await session.setStoredFavorite(work, enabled: !work.favorite) }
                }
                if work.favorite { Button(tr("设置分类")) { categories = true } }
                Button(tr("屏蔽作品")) { run { try await session.blockHistoryWork(work) } }
                Button(tr("屏蔽作者")) { authors = true }
                Button(tr("移除记录"), role: .destructive) { removing = true }
            }.disabled(busy)
            .alert(tr("移除记录"), isPresented: $removing) {
                Button(tr("取消"), role: .cancel) {}
                Button(tr("移除记录"), role: .destructive) { run { try await session.removeHistory([work]) } }
            } message: { Text(work.title + "\n" + tr("从继续阅读与历史中移除，章节的已读状态会保留。")) }
            .alert(tr("操作失败，请重试"), isPresented: $failed) { Button(tr("确定")) {} }
            .sheet(isPresented: $categories) { DownloadCategorySheet(work: work, session: session, tr: tr, favoriteOnly: true) }
            .sheet(isPresented: $authors) { HistoryAuthors(work: work,session: session,tr: tr) }
            .sheet(isPresented: $interactions) {
                if let library = InstalledSources.library { HistorySiteInteractions(work: work,session: session,library: library,tr: tr) }
            }
        } else { content }
    }
    private func run(_ action: @escaping () async throws -> Void) {
        guard !busy else { return }
        busy = true
        Task {
            defer { busy = false }
            do { try await action() } catch { failed = true }
        }
    }
}

@MainActor private struct HistoryAuthors: View {
    let work: LibraryWork
    let session: ProgressSession
    let tr: (String)->String
    @Environment(\.dismiss) private var dismiss
    @StateObject private var prompt = SitePrompt()
    @State private var authors: [String]?
    @State private var chosen: String?
    @State private var failed = false
    @State private var busy = false
    @State private var revision = 0
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(tr("屏蔽作者")).font(.headline)
            Text(work.title)
            if let authors {
                if authors.isEmpty { Text(tr("本站未提供作者信息")) }
                else {
                    Picker(tr("作者"), selection: $chosen) {
                        ForEach(authors,id: \.self) { Text($0).tag(Optional($0)) }
                    }
                }
            } else if !failed { ProgressView() }
            if failed { Button(tr("操作失败，请重试")) { revision += 1 } }
            HStack {
                Button(tr("取消")) { dismiss() }.keyboardShortcut(.cancelAction)
                Spacer()
                Button(tr("确定")) {
                    guard let chosen else { return }
                    busy = true
                    Task {
                        defer { busy = false }
                        do { try await session.blockHistoryWork(work, author: chosen); dismiss() } catch { failed = true }
                    }
                }.keyboardShortcut(.defaultAction).disabled(chosen == nil)
            }
        }.padding().disabled(busy).interactiveDismissDisabled(busy)
        .task(id: revision) {
            failed = false
            do {
                var values = try await session.storedAuthors(work)
                if values.isEmpty, let library = InstalledSources.library,
                   let site = try await session.installedSites(library).first(where: { $0.key == work.source }), site.detail {
                    let item = try await prompt.perform { try await session.siteDetail(library,site:work.source,id:work.workId,confirmation:$0) }
                    values = item.authors ?? []
                }
                try Task.checkCancellation()
                var seen = Set<String>()
                authors = values.map { $0.trimmingCharacters(in:.whitespacesAndNewlines) }.filter { !$0.isEmpty && seen.insert($0).inserted }
                chosen = authors?.first
            } catch { if !Task.isCancelled { failed = true } }
        }.siteConfirmation(prompt,tr:tr)
    }
}

@MainActor private struct HistorySiteInteractions: View {
    let work: LibraryWork
    let session: ProgressSession
    let library: SourceLibrary
    let tr: (String)->String
    @Environment(\.dismiss) private var dismiss
    @State private var interactions: [ProgressSession.Interaction]?
    @State private var error: String?
    @State private var busy = false
    @State private var rating = 3.0
    @State private var reload = 0
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(tr("原站互动")).font(.headline); Text(work.title)
            if let interactions {
                ForEach(interactions) { item in
                    if item.type == "rating" {
                        Stepper(value:$rating,in:0.5...5,step:0.5) { Text(tr("原站评分") + " · " + String(rating)) }
                        Button(tr("原站评分")) { update(.rating(stars: rating)) }
                    } else {
                        let active = item.value == .flag(true)
                        Toggle(tr(item.type == "like" ? "原站点赞" : "原站收藏"), isOn: Binding(get: { active },set: { on in
                            update(item.type == "like" ? .like(enabled:on) : .favorite(enabled:on))
                        })).disabled(active && item.canUnset == false)
                    }
                }
            } else if error == nil { ProgressView() }
            if let error {
                Text(tr(error))
                if error != "本站未提供原站互动" { Button(tr("重试")) { reload += 1 } }
            }
            Button(tr("完成")) { dismiss() }.keyboardShortcut(.cancelAction)
        }.padding().disabled(busy).interactiveDismissDisabled(busy)
        .task(id: reload) { await perform(nil) }
    }
    private func update(_ change: SitePreference) { guard !busy else { return }; Task { await perform(change) } }
    private func perform(_ change: SitePreference?) async {
        guard !busy else { return }
        busy = true; error = nil
        defer { busy = false }
        do {
            let next = try await session.siteInteractions(library,work:work,change:change)
            try Task.checkCancellation()
            interactions = next
            if case .number(let value) = next.first(where: { $0.type == "rating" })?.value { rating = NSDecimalNumber(decimal:value).doubleValue }
        } catch SourceFailure.Unsupported { self.error = "本站未提供原站互动" }
        catch { if !Task.isCancelled { self.error = "操作失败，请重试" } }
    }
}

@MainActor struct HistoryShelf: View {
    enum Scope: Hashable { case all, domain(String) }
    let session: ProgressSession
    let locale: String
    let incognito: Bool
    let tr: (String) -> String
    @Environment(\.dismiss) private var dismiss
    @State private var works = [LibraryWork]()
    @State private var scope: Scope
    @State private var source: String?
    @State private var sourceDomains = [String: String]()
    @State private var query = ""
    @State private var grid = false
    @State private var loading = true
    @State private var failed = false
    @State private var busy = false
    @State private var selecting = false
    @State private var selected = Set<LibraryWorkID>()
    @State private var target: LibraryWork?
    @State private var removal = [LibraryWork]()
    @State private var confirming = false
    @State private var revision = 0

    init(session: ProgressSession, locale: String, incognito: Bool, tr: @escaping (String) -> String, scope: Scope = .all) {
        self.session = session
        self.locale = locale
        self.incognito = incognito
        self.tr = tr
        self._scope = State(initialValue: scope)
    }

    private func domain(_ work: LibraryWork) -> String? {
        sourceDomains[work.source] ?? appleCategoryOptions.first(where: { $0.id == work.contentDomain })?.id
    }

    private var domainWorks: [LibraryWork] {
        works.filter { work in
            switch scope {
            case .all: return true
            case .domain(let domain): return self.domain(work) == domain
            }
        }
    }

    private var scoped: [LibraryWork] {
        guard let source else { return domainWorks }
        return domainWorks.filter { $0.source == source }
    }

    private var visible: [LibraryWork] {
        let term = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return scoped.filter { term.isEmpty || $0.title.localizedCaseInsensitiveContains(term) }
    }

    private func clearSelection() {
        selected.removeAll()
        selecting = false
    }

    private func mark(_ work: LibraryWork) {
        selecting = true
        if !selected.insert(work.libraryID).inserted { selected.remove(work.libraryID) }
    }

    private func propose(_ rows: [LibraryWork]) {
        guard !rows.isEmpty else { return }
        removal = rows
        confirming = true
    }

    var body: some View {
        Group {
            if let work = target {
                StoredSiteDetails(session: session, source: work.source, workId: work.workId, title: work.title,
                                  locale: locale, incognito: incognito, tr: tr, back: { target = nil })
            } else {
                VStack(alignment: .leading, spacing: 12) {
                    HStack {
                        Button(tr("返回")) { if selecting { clearSelection() } else { dismiss() } }.keyboardShortcut(.cancelAction)
                        Text(tr("浏览历史")).font(.headline)
                        Spacer()
                        if selecting {
                            Button(tr("取消选择"), action: clearSelection)
                            Button(tr("删除"), role: .destructive) { propose(scoped.filter { selected.contains($0.libraryID) }) }
                                .disabled(selected.isEmpty)
                        } else {
                            Menu(tr("更多")) {
                                Button(tr("批量选择")) { selecting = true }
                                Button(tr(grid ? "切换为列表" : "切换为网格")) {
                                    busy = true
                                    Task {
                                        defer { busy = false }
                                        do { try await session.setHistoryGrid(!grid); grid.toggle() }
                                        catch { failed = true }
                                    }
                                }
                                Button(tr("清空历史"), role: .destructive) { propose(scoped) }.disabled(scoped.isEmpty)
                            }
                        }
                    }
                    Picker(tr("内容范围"), selection: $scope) {
                        Text(tr("全部")).tag(Scope.all)
                        ForEach(appleCategoryOptions.filter { option in works.contains { domain($0) == option.id } }) { option in
                            Text(tr(option.label)).tag(Scope.domain(option.id))
                        }
                    }.pickerStyle(.menu).onChange(of: scope) { _ in source = nil; clearSelection() }
                    Picker(tr("站点"), selection: $source) {
                        Text(tr("全部")).tag(String?.none)
                        ForEach(Array(Set(domainWorks.map(\.source))).sorted(), id: \.self) { key in
                            Text(key).tag(Optional(key))
                        }
                    }.pickerStyle(.menu).onChange(of: source) { _ in clearSelection() }
                    TextField(tr("搜索历史"), text: $query).textFieldStyle(.roundedBorder)
                        .onChange(of: query) { _ in clearSelection() }
                    if failed { Button(tr("操作失败，请重试")) { revision += 1 } }
                    if loading { ProgressView() }
                    else if visible.isEmpty { Text(tr("没有匹配的记录")).foregroundColor(.secondary); Spacer() }
                    else if grid {
                        ScrollView {
                            LazyVGrid(columns: [GridItem(.adaptive(minimum: 140), alignment: .leading)], alignment: .leading, spacing: 14) {
                                ForEach(visible, id: \.libraryID) { work in row(work) }
                            }
                        }
                    } else {
                        List(visible, id: \.libraryID) { work in row(work) }
                    }
                }.padding()
            }
        }
        #if os(macOS)
        .frame(minWidth: 420, idealWidth: 560, minHeight: 320, idealHeight: 440)
        #endif
        .disabled(busy)
        .interactiveDismissDisabled(selecting || busy || target != nil)
        .onReceive(NotificationCenter.default.publisher(for: .parkHistoryChanged)) { _ in revision += 1 }
        .task(id: revision) {
            do {
                let rows = try await session.libraryWorks().filter { $0.historyMs != nil }
                let layout = try await session.historyGrid()
                var domains = [String: String]()
                if let library = InstalledSources.library, let sites = try? await session.installedSites(library) {
                    for site in sites { domains[site.key] = site.category }
                }
                try Task.checkCancellation()
                sourceDomains = domains; works = rows; grid = layout; failed = false
                selected.formIntersection(rows.map(\.libraryID))
            } catch { if !Task.isCancelled { failed = true } }
            if !Task.isCancelled { loading = false }
        }
        .alert(tr(selecting ? "删除选中的历史记录？" : "清空历史记录？"), isPresented: $confirming) {
            Button(tr("取消"), role: .cancel) { removal.removeAll() }
            Button(tr("删除"), role: .destructive) {
                let snapshot = removal
                busy = true
                Task {
                    defer { busy = false; removal.removeAll() }
                    do { try await session.removeHistory(snapshot); clearSelection() }
                    catch { failed = true }
                }
            }
        } message: {
            Text(tr("从继续阅读与历史中移除，章节的已读状态会保留。") + "\n" + String(removal.count))
        }
    }

    private func row(_ work: LibraryWork) -> some View {
        Button {
            if selecting { mark(work) } else { target = work }
        } label: {
            HStack {
                if selecting { Image(systemName: selected.contains(work.libraryID) ? "checkmark.circle.fill" : "circle") }
                VStack(alignment: .leading) {
                    Text(work.title)
                    Text(work.source).font(.caption).foregroundColor(.secondary)
                }
                Spacer()
            }.contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(Text(work.title))
        .accessibilityAddTraits(selected.contains(work.libraryID) ? [.isSelected] : [])
        .contextMenu { Button(tr("批量选择")) { mark(work) } }
    }
}
