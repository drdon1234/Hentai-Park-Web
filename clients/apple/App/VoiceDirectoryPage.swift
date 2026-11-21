import SwiftUI
import ParkCore

@MainActor struct VoiceDirectoryPage: View {
  let session: ProgressSession
  let library: SourceLibrary
  let site: String
  let work: SiteWork
  let folder: VoiceNode?
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void

  @State private var listing: VoiceFolder?
  @State private var child: VoiceNode?
  @State private var playOnOpen = true
  @State private var active: ChapterKey?
  @State private var attachment: VoiceNode?
  @State private var attempt = 0
  @State private var loadedAttempt: Int?
  @State private var loading = false
  @State private var selecting = false
  @State private var problem: String?
  @StateObject private var prompt = SitePrompt()

  var body: some View {
    if let child {
      VoiceDirectoryPage(session: session, library: library, site: site, work: work, folder: child,
        incognito: incognito, tr: tr, back: { self.child = nil }).id(child.id)
    } else if let active, let listing, let track = listing.entries.first(where: { $0.id == active.chapterId }) {
      MovieReader(key: active, title: track.name, incognito: incognito, showBack: true, tr: tr,
        back: { self.active = nil; playOnOpen = true }, library: library, captions: listing.subtitles(for: track),
        tracks: listing.entries.filter(\.playable), playOnOpen: playOnOpen, selectTrack: { node, resume in
          playOnOpen = resume
          self.active = ChapterKey(source: active.source, workId: active.workId, chapterId: node.id)
        }).id(active)
    } else {
      VStack(alignment: .leading) {
        Button(tr("返回"), action: back).padding(.horizontal)
        Text(folder?.name ?? work.title).font(.headline).padding(.horizontal)
        List {
          ForEach(listing?.entries ?? []) { node in
            if node.type == "directory" {
              Button(node.name) { child = node }
            } else if node.playable {
              Button(node.name) {
                guard let listing, !selecting else { return }
                selecting = true
                Task {
                  defer { selecting = false }
                  do {
                    active = try await session.selectVoiceTrack(work: work, site: site, folder: listing,
                      track: node.id, incognito: incognito)
                  } catch { if !Task.isCancelled { problem = "保存失败，请重试" } }
                }
              }.disabled(selecting)
            } else if node.type == "subtitle" {
              Button(node.name) { attachment = node }
            } else { Text(node.name) }
          }
          if loading { ProgressView() }
          if let problem {
            Text(tr(problem))
          }
          if !loading && (listing?.nextPage != nil || loadedAttempt != attempt && problem != nil) {
            Button(tr(problem == nil ? "加载更多" : "重试")) { attempt += 1 }
          }
          if listing?.entries.isEmpty == true && !loading && problem == nil { Text(tr("暂无内容")) }
        }
      }
      .task(id: attempt) {
        guard loadedAttempt != attempt else { return }
        loading = true
        problem = nil
        defer { if !Task.isCancelled { loading = false } }
        do {
          let current = try await prompt.perform { ticket in
            try await session.voiceFolder(library, site: site, work: work, folder: folder,
              after: listing, confirmation: ticket)
          }
          try Task.checkCancellation()
          listing = current
          loadedAttempt = attempt
        } catch { if !Task.isCancelled { problem = siteMessage(error) } }
      }
      .siteConfirmation(prompt, tr: tr)
      .sheet(item: $attachment) { node in
        VoiceCaptionPage(session: session, library: library, key: ChapterKey(source: site, workId: work.id, chapterId: node.id),
          node: node, tr: tr, dismiss: { attachment = nil }, seek: nil)
      }
    }
  }
}

@MainActor struct VoiceCaptionPage: View {
  let session: ProgressSession
  let library: SourceLibrary?
  let key: ChapterKey
  let node: VoiceNode
  let tr: (String) -> String
  let dismiss: () -> Void
  let seek: ((Int64) -> Void)?
  @State private var attempt = 0
  @State private var cues: [VoiceCue]?
  @State private var failed = false

  var body: some View {
    VStack(alignment: .leading) {
      Button(tr("返回"), action: dismiss).padding(.horizontal)
      Text(node.name).font(.headline).padding(.horizontal)
      if let cues {
        List(cues.indices, id: \.self) { index in
          let cue = cues[index]
          if let seek {
            Button(cue.text) { seek(cue.startMs); dismiss() }
          } else { Text(cue.text) }
        }
      } else if failed { Button(tr("重试")) { attempt += 1 } } else { ProgressView() }
    }.task(id: attempt) {
      do {
        failed = false
        cues = try await session.voiceCues(library, key: key, node: node.id)
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}

@MainActor struct StoredVoiceReader: View {
  let session: ProgressSession
  let library: SourceLibrary
  let key: ChapterKey
  let title: String
  let incognito: Bool
  let showBack: Bool
  let tr: (String) -> String
  let back: () -> Void
  @State private var folder: VoiceFolder?
  @State private var selected: VoiceNode?
  @State private var playOnOpen = true
  @State private var failed = false
  @State private var attempt = 0

  var body: some View {
    Group {
      if let folder, let selected {
        let current = ChapterKey(source: key.source, workId: key.workId, chapterId: selected.id)
        MovieReader(key: current, title: selected.name, incognito: incognito, showBack: showBack, tr: tr,
          back: back, library: library, captions: folder.subtitles(for: selected),
          tracks: folder.entries.filter(\.playable), playOnOpen: playOnOpen, selectTrack: { track, resume in
            playOnOpen = resume
            self.selected = track
          }).id(current)
      } else if failed {
        Button(tr("返回"), action: back)
        Button(tr("重试")) { attempt += 1 }
      } else { ProgressView() }
    }.task(id: attempt) {
      guard folder == nil else { return }
      failed = false
      do {
        let snapshot = try await session.storedVoiceFolder(key, title: title)
        guard let node = snapshot.entries.first(where: { $0.id == key.chapterId && $0.playable })
        else { throw SiteReadError.malformed }
        try Task.checkCancellation()
        selected = node
        folder = snapshot
      } catch { if !Task.isCancelled { failed = true } }
    }
  }
}
