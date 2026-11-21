import AVKit
import ParkCore
import SwiftUI

@MainActor
private final class MoviePlayback: ObservableObject, PlaybackOwner {
  struct Line {
    let index: Int
    let label: String
  }
  @Published private(set) var player: AVPlayer?
  @Published private(set) var web: WebPlayback?
  @Published private(set) var webPaused = true
  @Published private(set) var failed = false
  @Published private(set) var saveFailed = false
  @Published private(set) var options: [VideoLine] = []
  @Published private(set) var selectedLine: Int?
  private(set) var session: ProgressSession?
  private var route: PlaybackSession?
  private var preparingAsset: AVURLAsset?
  private var preparingPlayer: AVPlayer?
  private var statusObservation: NSKeyValueObservation?
  private var identity = UUID()
  private var key: ChapterKey?
  private var title = ""
  private var incognito = false
  private var stamp: Int64 = 0
  private var lastPosition: Int64?
  private var duration: Int64?
  private var savedDuration: Int64?
  private var pending: Task<Bool, Never>?
  private var draining: Task<Void, Never>?

  func load(key: ChapterKey, title: String, incognito: Bool, library: SourceLibrary?, bundle: ChapterKey?,
            recording: RecordingVolume?, line: Line?)
    async
  {
    let closed = close()
    let generation = identity
    self.incognito = incognito
    failed = false
    saveFailed = false
    var opened: ProgressSession?
    var acquired: PlaybackSession?
    do {
      await closed?.value
      try Task.checkCancellation()
      guard generation == identity else { throw CancellationError() }
      let session = try await ProgressSession.openApplicationLibrary()
      opened = session
      let previous = recording == nil ? try await session.read(key) : nil
      let route: PlaybackSession
      if let recording {
        route = try await session.recordingMedia(recording)
      } else if let bundle {
        route = try await session.bundleMedia(bundle, node: key.chapterId)
      } else if let library {
        let choices = try await session.videoChoices(library, key: key)
        if let web = choices.webPlayback() {
          try Task.checkCancellation()
          guard identity == generation else { throw CancellationError() }
          self.session = session
          self.web = web
          return
        }
        let lines = choices.options()
        guard !lines.isEmpty else { throw SourceFailure.Unsupported }
        let index =
          line.flatMap { saved in
            if lines.indices.contains(saved.index), lines[saved.index].label == saved.label {
              return saved.index
            }
            return lines.firstIndex { $0.label == saved.label }
          } ?? lines.firstIndex(where: { $0.preferred }) ?? 0
        route = try await session.stream(choices, index: Int32(index))
        acquired = route
        try Task.checkCancellation()
        guard identity == generation else { throw CancellationError() }
        options = lines
        selectedLine = index
      } else {
        route = try await session.offlineVideo(key)
      }
      acquired = route
      try Task.checkCancellation()
      guard identity == generation else { throw CancellationError() }
      guard let url = URL(string: route.localUrl()), url.host == "127.0.0.1" else {
        throw CocoaError(.fileReadCorruptFile)
      }
      let policy: AVAssetReferenceRestrictions =
        url.pathExtension == "m3u8"
        ? [.forbidCrossSiteReference, .forbidRemoteReferenceToLocal, .forbidLocalReferenceToRemote]
        : .forbidAll
      let asset = AVURLAsset(
        url: url, options: [AVURLAssetReferenceRestrictionsKey: NSNumber(value: policy.rawValue)])
      preparingAsset = asset
      let time = try await asset.load(.duration)
      try Task.checkCancellation()
      guard identity == generation else { throw CancellationError() }
      let player = AVPlayer(playerItem: AVPlayerItem(asset: asset))
      preparingPlayer = player
      let end = playbackSeconds(time.seconds).flatMap { $0 > 0 ? $0 : nil }
      let start = min(end ?? Int64.max, max(0, previous?.positionSeconds ?? 0))
      if start > 0 {
        let reached: Bool = await withCheckedContinuation { done in
          player.seek(
            to: CMTime(value: start, timescale: 1), toleranceBefore: .zero, toleranceAfter: .zero
          ) {
            done.resume(returning: $0)
          }
        }
        guard reached else { throw CancellationError() }
      }
      try Task.checkCancellation()
      guard identity == generation else { throw CancellationError() }
      self.session = session
      self.route = route
      self.key = recording == nil ? key : nil
      self.title = title
      self.duration = end
      self.savedDuration = previous.flatMap { $0.durationSeconds > 0 ? $0.durationSeconds : nil }
      self.stamp = previous?.positionUpdatedMs ?? 0
      self.lastPosition = start
      self.player = player
      statusObservation = player.observe(\.timeControlStatus, options: [.new]) {
        [weak self] player, _ in
        Task { @MainActor [weak self, weak player] in
          guard let self, let player, self.player === player else { return }
          if player.timeControlStatus == .paused {
            PlaybackFocus.shared.release(self)
          } else {
            do { try PlaybackFocus.shared.claim(self, spoken: false) } catch {
              self.interrupt()
              self.failed = true
            }
          }
        }
      }
      preparingAsset = nil
      preparingPlayer = nil
    } catch {
      if identity == generation { cancelPreparation() }
      acquired?.revoke()
      await opened?.close()
      if identity == generation && !Task.isCancelled { failed = true }
    }
  }

  @discardableResult
  func checkpoint() -> Task<Bool, Never>? {
    guard let player, let session, let key, !incognito else { return nil }
    if let current = player.currentItem, let measured = playbackSeconds(current.duration.seconds), measured > 0 {
      duration = measured
    }
    guard let seconds = playbackSeconds(player.currentTime().seconds, duration: duration) else { return pending }
    if seconds == lastPosition && (duration == nil || duration == savedDuration) { return pending }
    stamp = max(stamp + 1, Int64(Date().timeIntervalSince1970 * 1000))
    let value = MediaPosition(
      key: key, chapterTitle: title, seconds: seconds, durationSeconds: duration, updatedMs: stamp)
    let earlier = pending
    let generation = identity
    lastPosition = seconds
    savedDuration = duration ?? savedDuration
    let save = Task { [weak self] in
      _ = await earlier?.value
      do {
        _ = try await session.savePosition(value)
        if self?.identity == generation { self?.saveFailed = false }
        return true
      } catch {
        if self?.identity == generation && self?.stamp == value.updatedMs {
          self?.lastPosition = nil
          self?.savedDuration = nil
          self?.saveFailed = true
        }
        return false
      }
    }
    pending = save
    return save
  }

  func interrupt() {
    webPaused = true
    player?.pause()
    PlaybackFocus.shared.release(self)
    checkpoint()
  }

  func startWeb() {
    guard web != nil else { return }
    do {
      try PlaybackFocus.shared.claim(self, spoken: false)
      webPaused = false
    } catch { failed = true }
  }

  func setIncognito(_ enabled: Bool) {
    incognito = enabled
    if let time = player?.currentTime().seconds {
      lastPosition = playbackSeconds(time, duration: duration)
      savedDuration = duration
    }
  }

  func tick() {
    if player?.currentItem?.status == .failed {
      interrupt()
      failed = true
    } else {
      checkpoint()
    }
  }

  @discardableResult func close() -> Task<Void, Never>? {
    interrupt()
    let saved = pending
    let opened = session
    identity = UUID()
    statusObservation?.invalidate()
    statusObservation = nil
    cancelPreparation()
    player?.replaceCurrentItem(with: nil)
    player = nil
    web = nil
    route?.revoke()
    route = nil
    session = nil
    pending = nil
    lastPosition = nil
    options = []
    selectedLine = nil
    if saved != nil || opened != nil {
      let earlier = draining
      draining = Task {
        await earlier?.value
        _ = await saved?.value
        await opened?.close()
      }
    }
    return draining
  }

  private func cancelPreparation() {
    preparingPlayer?.currentItem?.cancelPendingSeeks()
    preparingPlayer?.replaceCurrentItem(with: nil)
    preparingAsset?.cancelLoading()
    preparingPlayer = nil
    preparingAsset = nil
  }
}

@MainActor
struct MovieReader: View {
  let key: ChapterKey
  let title: String
  let incognito: Bool
  let showBack: Bool
  let tr: (String) -> String
  let back: () -> Void
  var library: SourceLibrary? = nil
  var bundle: ChapterKey? = nil
  var recording: RecordingVolume? = nil
  var captions: [VoiceNode] = []
  var tracks: [VoiceNode] = []
  var playOnOpen = true
  var selectTrack: ((VoiceNode, Bool) -> Void)? = nil
  @State private var transitioned = false
  @State private var caption: VoiceNode?
  @StateObject private var playback = MoviePlayback()
  @State private var retry = 0
  @State private var line: MoviePlayback.Line?
  @State private var changing = false
  @State private var webReady = false
  @State private var webFailed = false
  @Environment(\.scenePhase) private var phase
  @Environment(\.privacyObscured) private var obscured
  var body: some View {
    VStack {
      HStack {
        if showBack {
          Button(tr("返回")) {
            playback.interrupt()
            Task { if await playback.checkpoint()?.value != false { back() } }
          }
        }
        Text(title).lineLimit(2)
        Spacer()
        if playback.options.count > 1, let selected = playback.selectedLine {
          Menu(playback.options[selected].label) {
            ForEach(playback.options.indices, id: \.self) { index in
              Button(playback.options[index].label) {
                guard selected != index, !changing else { return }
                let choice = MoviePlayback.Line(index: index, label: playback.options[index].label)
                changing = true
                playback.interrupt()
                Task {
                  defer { changing = false }
                  if await playback.checkpoint()?.value != false {
                    line = choice
                    retry += 1
                  }
                }
              }
            }
          }.disabled(changing)
        }
      }.padding()
      if playback.failed {
        Text(tr("媒体播放失败"))
        Button(tr("重试")) { retry += 1 }
      } else if let web = playback.web {
        IsolatedSitePage(
          policy: .video(web), suspended: playback.webPaused || obscured || phase != .active,
          ready: $webReady, failed: $webFailed
        )
        .allowsHitTesting(
          webReady && !webFailed && !playback.webPaused && !obscured && phase == .active)
        if webFailed {
          Button(tr("媒体播放失败") + " · " + tr("重试")) { retry += 1 }
        } else if !webReady {
          ProgressView()
        } else if playback.webPaused {
          Button(tr("播放")) { playback.startWeb() }.disabled(obscured || phase != .active)
        }
      } else if let player = playback.player {
        VideoPlayer(player: player)
          .onReceive(NotificationCenter.default.publisher(for: .AVPlayerItemDidPlayToEndTime)) { event in
            guard !obscured, phase == .active, let item = event.object as? AVPlayerItem,
              item === player.currentItem, let index = tracks.firstIndex(where: { $0.id == key.chapterId }),
              index + 1 < tracks.count else { return }
            changeTrack(tracks[index + 1], play: true)
          }
        if tracks.count > 1 {
          Menu(tr("当前曲目")) {
            ForEach(tracks) { track in
              Button(track.name) { changeTrack(track) }.disabled(track.id == key.chapterId || transitioned)
            }
          }
        }
        if let session = playback.session, !captions.isEmpty {
          Menu(tr("字幕")) {
            ForEach(captions) { node in Button(node.name) { caption = node } }
          }.sheet(item: $caption) { node in
            VoiceCaptionPage(session: session, library: library, key: bundle ?? key, node: node, tr: tr,
              dismiss: { caption = nil }, seek: { milliseconds in
                player.seek(to: CMTime(value: milliseconds, timescale: 1000), toleranceBefore: .zero, toleranceAfter: .zero)
              })
          }
        }
        if playback.saveFailed { Button(tr("保存失败，请重试")) { playback.checkpoint() } }
      } else {
        ProgressView()
      }
    }
    .task(id: retry) {
      webReady = false
      webFailed = false
      await playback.load(
        key: key, title: title, incognito: incognito, library: library, bundle: bundle, recording: recording, line: line)
      while !Task.isCancelled && playback.player != nil && !playback.failed {
        do { try await Task.sleep(nanoseconds: 3_000_000_000) } catch { return }
        playback.tick()
      }
    }
    .onChange(of: playback.player) { player in
      if !tracks.isEmpty && playOnOpen && !obscured && phase == .active { player?.play() }
    }
    .onChange(of: phase) { if $0 != .active { playback.interrupt() } }
    .onChange(of: obscured) { if $0 { playback.interrupt() } }
    .onChange(of: incognito) { playback.setIncognito($0) }
    #if os(iOS)
      .onReceive(NotificationCenter.default.publisher(for: AVAudioSession.interruptionNotification))
      { event in
        if (event.userInfo?[AVAudioSessionInterruptionTypeKey] as? NSNumber)?.uintValue
          == AVAudioSession.InterruptionType.began.rawValue
        {
          playback.interrupt()
        }
      }
    #endif
    .onDisappear { playback.close() }
  }
  private func changeTrack(_ track: VoiceNode, play: Bool? = nil) {
    guard !transitioned, let selectTrack else { return }
    let resume = play ?? (playback.player?.timeControlStatus != .paused && playback.player != nil)
    transitioned = true
    playback.interrupt()
    Task {
      defer { transitioned = false }
      if await playback.checkpoint()?.value != false { selectTrack(track, resume) }
    }
  }

}
