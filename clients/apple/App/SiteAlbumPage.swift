import ParkCore
import SwiftUI

@MainActor struct SiteAlbumPage: View {
    let session: ProgressSession
    let library: SourceLibrary
    let site: InstalledSite
    let scope: Data
    let locale: String
    let incognito: Bool
    let tr: (String) -> String
    let close: () -> Void
    @StateObject private var prompt = SitePrompt()
    @State private var album = SiteAlbum()
    @State private var media = "all"
    @State private var columns = 3
    @State private var collapsed = false
    @State private var saving = false
    @State private var dates: ClosedRange<Date>?
    @State private var pickingDates = false
    @State private var selected: AlbumEntry?
    @State private var failure: String?
    @State private var settingsFailed = false
    @State private var loading = false
    @State private var reset = true
    @State private var generation = 0

    private func matching(_ entry: AlbumEntry) -> [AlbumAttachment] {
        guard entry.matches(from: dates?.lowerBound, through: dates?.upperBound) else { return [] }
        return entry.media.filter { media == "all" || $0.kind == media }
    }

    private func fetch(reset: Bool) {
        self.reset = reset
        generation += 1
    }

    private func fillEmptyFilter() {
        if album.cursor != nil && !loading && album.entries.allSatisfy({ matching($0).isEmpty }) { fetch(reset: false) }
    }

    var body: some View {
        VStack(alignment: .leading) {
            HStack {
                Button(tr("返回"), action: close).keyboardShortcut(.cancelAction)
                Text((site.name ?? site.key) + " · " + tr("相册")).font(.headline)
                Spacer()
                Button(tr("刷新")) { fetch(reset: true) }.keyboardShortcut("r", modifiers: .command)
            }
            HStack {
                Picker(tr("媒体类型"), selection: $media) {
                    Text(tr("全部")).tag("all"); Text(tr("照片")).tag("image"); Text(tr("视频")).tag("video")
                }.pickerStyle(.menu)
                Button(tr("日期范围")) { pickingDates = true }
                if dates != nil { Button(tr("清除日期")) { dates = nil } }
                Menu(tr("更多")) {
                    Picker(tr("相册列数"), selection: Binding(get: { columns }, set: { number in
                        save { try await session.saveAlbumColumns(number); columns = number }
                    })) { ForEach(1...6, id: \.self) { Text(String($0)).tag($0) } }
                    Toggle(tr("收起同一作品的媒体"), isOn: Binding(get: { collapsed }, set: { value in
                        save { try await session.saveAlbumCollapsed(value); collapsed = value }
                    }))
                }.disabled(saving)
            }
            if let dates {
                Text(dates.lowerBound.formatted(date: .abbreviated, time: .omitted) + " – " +
                     dates.upperBound.formatted(date: .abbreviated, time: .omitted)).font(.caption)
            }
            if settingsFailed { Text(tr("保存失败，请重试")) }
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 16) {
                    ForEach(album.entries) { entry in
                        let attachments = matching(entry)
                        if !attachments.isEmpty {
                            Button(entry.title) { selected = entry }
                            if let date = entry.date { Text(date, style: .date).font(.caption) }
                            LazyVGrid(columns: Array(repeating: GridItem(.flexible(minimum: 16)), count: columns)) {
                                ForEach(collapsed ? Array(attachments.prefix(1)) : attachments) { attachment in
                                    AlbumPreviewTile(session: session, library: library, site: site.key,
                                                     attachment: attachment, title: entry.title, tr: tr) { selected = entry }
                                }
                            }
                            if collapsed && attachments.count > 1 {
                                Text(tr("已收起 {count} 项").replacingOccurrences(of: "{count}", with: String(attachments.count - 1))).font(.caption)
                            }
                        }
                    }
                    if let failure {
                        Text(tr(failure)); Button(tr("重试")) { fetch(reset: reset) }
                    } else if loading { ProgressView() }
                    else {
                        if album.entries.allSatisfy({ matching($0).isEmpty }) { Text(tr("当前筛选下没有媒体")) }
                        if album.cursor != nil { Button(tr("加载更多")) { fetch(reset: false) } }
                    }
                }
            }.refreshable { fetch(reset: true) }
        }.padding()
        #if os(macOS)
        .frame(minWidth: 460, idealWidth: 720, minHeight: 400, idealHeight: 540)
        #endif
        .task {
            do {
                let layout = try await session.albumLayout()
                columns = min(6, max(1, layout.albumColumns ?? 3))
                collapsed = layout.albumCollapsed ?? false
            } catch { if !Task.isCancelled { settingsFailed = true } }
        }
        .task(id: generation) {
            loading = true; failure = nil
            defer { if !Task.isCancelled { loading = false } }
            do {
                var previous = reset ? SiteAlbum() : album
                for _ in 0..<4 {
                    let result = try await prompt.perform { ticket in
                        try await session.albumPage(library, site: site, scope: scope, prior: previous, confirmation: ticket)
                    }
                    try Task.checkCancellation()
                    album = result; previous = result; reset = false
                    if result.cursor == nil || result.entries.contains(where: { !matching($0).isEmpty }) { break }
                }
            } catch { if !Task.isCancelled { failure = siteMessage(error) } }
        }
        .onChange(of: dates) { _ in fillEmptyFilter() }.onChange(of: media) { _ in fillEmptyFilter() }
        .siteConfirmation(prompt, tr: tr)
        .sheet(isPresented: $pickingDates) {
            AlbumDates(dates: dates, tr: tr) { dates = $0; pickingDates = false }
        }
        .sheet(item: $selected) { entry in
            AlbumDestination(session: session, library: library, site: site, entry: entry, locale: locale,
                             incognito: incognito, tr: tr, close: { selected = nil })
        }
    }
    private func save(_ action: @escaping () async throws -> Void) {
        guard !saving else { return }
        saving = true
        Task {
            defer { saving = false }
            do { try await action(); settingsFailed = false } catch { settingsFailed = true }
        }
    }
}

@MainActor private struct AlbumDates: View {
    let tr: (String) -> String
    let apply: (ClosedRange<Date>) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var start: Date
    @State private var end: Date
    init(dates: ClosedRange<Date>?, tr: @escaping (String)->String, apply: @escaping (ClosedRange<Date>)->Void) {
        self.tr = tr; self.apply = apply
        let today = Calendar.current.startOfDay(for: Date())
        _start = State(initialValue: dates?.lowerBound ?? today)
        _end = State(initialValue: dates?.upperBound ?? today)
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(tr("日期范围")).font(.headline)
            DatePicker(tr("开始日期"), selection: $start, displayedComponents: .date)
            DatePicker(tr("结束日期"), selection: $end, displayedComponents: .date)
            HStack {
                Button(tr("取消")) { dismiss() }.keyboardShortcut(.cancelAction)
                Spacer()
                Button(tr("确定")) { apply(start...end) }.disabled(start > end).keyboardShortcut(.defaultAction)
            }
        }.padding()
    }
}

@MainActor private struct AlbumDestination: View {
    let session: ProgressSession
    let library: SourceLibrary
    let site: InstalledSite
    let entry: AlbumEntry
    let locale: String
    let incognito: Bool
    let tr: (String) -> String
    let close: () -> Void
    @State private var post: SiteWork?
    @State private var failed = false
    @State private var attempt = 0
    var body: some View {
        Group {
            if entry.presentation == "work" || post != nil {
                SiteDetails(session: session, library: library, site: site, workId: entry.id, preview: nil,
                            locale: locale, incognito: incognito, tr: tr, openedDetail: post, close: close)
            } else if failed {
                Button(tr("返回"), action: close); Button(tr("重试")) { attempt += 1 }
            } else { ProgressView() }
        }.task(id: attempt) {
            guard entry.presentation == "post" else { return }
            do { post = try await session.albumPost(entry, source: site.key) }
            catch { if !Task.isCancelled { failed = true } }
        }
        #if os(macOS)
        .frame(minWidth: 400, minHeight: 320)
        #endif
    }
}

@MainActor private struct AlbumPreviewTile: View {
    let session: ProgressSession
    let library: SourceLibrary
    let site: String
    let attachment: AlbumAttachment
    let title: String
    let tr: (String) -> String
    let open: () -> Void
    @Environment(\.blurCovers) private var blurCovers
    @State private var pixels: CGImage?
    @State private var failure = false
    @State private var attempt = 0
    var body: some View {
        VStack {
            Button(action: open) {
                if let pixels {
                    Image(decorative: pixels, scale: 1).resizable().scaledToFit().blur(radius: blurCovers ? 14 : 0)
                } else { Label(tr(attachment.kind == "video" ? "视频" : "照片"), systemImage: attachment.kind == "video" ? "film" : "photo") }
            }.accessibilityLabel(Text(title + " · " + attachment.id))
            if failure { Button(tr("重试")) { attempt += 1 } }
        }.task(id: attempt) {
            failure = false
            do {
                if let asset = try await session.albumThumbnail(library, site: site, media: attachment) {
                    let image = try await decodedPage(asset, maxSize: 512)
                    try Task.checkCancellation()
                    pixels = image
                }
            } catch { if !Task.isCancelled { failure = true } }
        }.onDisappear { pixels = nil }
    }
}
