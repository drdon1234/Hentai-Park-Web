import Foundation

#if os(iOS)
  import AVFAudio
#endif

@MainActor public protocol PlaybackOwner: AnyObject {
  func interrupt()
}

@MainActor public final class PlaybackFocus {
  public static let shared = PlaybackFocus()
  private weak var owner: (any PlaybackOwner)?
  private init() {}

  public func claim(_ next: any PlaybackOwner, spoken: Bool) throws {
    if owner === next { return }
    let previous = owner
    owner = next
    previous?.interrupt()
    #if os(iOS)
      let session = AVAudioSession.sharedInstance()
      do {
        try session.setCategory(.playback, mode: spoken ? .spokenAudio : .moviePlayback)
        try session.setActive(true)
      } catch {
        release(next)
        throw error
      }
    #endif
  }

  public func release(_ current: any PlaybackOwner) {
    guard owner === current else { return }
    owner = nil
    #if os(iOS)
      try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    #endif
  }
}
