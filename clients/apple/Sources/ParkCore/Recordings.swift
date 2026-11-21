import Foundation

public struct RecordedBroadcast: Decodable, Identifiable, Sendable {
  public let id: String
  public let source: String
  public let work: String
  public let title: String
  public let status: String
  public let volumes: [UInt32]
  public let unavailable: Bool?
  public var libraryID: LibraryWorkID { LibraryWorkID(source: source, work: work) }
}

public struct RecordingVolume: Sendable {
  public let recording: RecordedBroadcast
  public let number: UInt32

  public init(_ recording: RecordedBroadcast, number: UInt32) {
    self.recording = recording
    self.number = number
  }
}

extension ProgressSession {
  public func captureBroadcast(_ library: SourceLibrary, work: LibraryWork) async throws {
    let root = directory
    let downloads = try downloadDirectory(configured: nil)
    try await run { store, request in
      let service = try library.service(directory: root, request: request)
      let recording = try service.createRecording(store: store, source: work.source,
        work: work.workId, title: work.title, defaultDirectory: downloads.path, request: request)
      try recording.capture(service: service, request: request)
    }
  }

  public func recordings() async throws -> [RecordedBroadcast] {
    try await run { store, _ in
      let json = try store.liveRecordings()
      return try JSONDecoder().decode([RecordedBroadcast].self, from: Data(json.utf8))
    }
  }

  public func recordingMedia(_ volume: RecordingVolume) async throws -> PlaybackSession {
    try await run { store, request in
      try store.playRecordingVolume(id: volume.recording.id, volume: volume.number, request: request)
    }
  }
}
