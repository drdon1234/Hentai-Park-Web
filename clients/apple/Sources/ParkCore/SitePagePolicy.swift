import Foundation

public enum SitePagePolicy: Sendable {
  case confirmation(URL)
  case game(OnlineGame)
  case video(WebPlayback)

  public var url: URL? {
    switch self {
    case .confirmation(let url): return url
    case .game(let game): return URL(string: game.url())
    case .video(let video): return URL(string: video.url())
    }
  }

  public func permits(_ address: URL?, subframe: Bool = false) -> Bool {
    guard let address else { return false }
    switch self {
    case .confirmation(let origin):
      return address.scheme?.lowercased() == "https"
        && address.host?.lowercased() == origin.host?.lowercased()
        && (address.port ?? 443) == 443 && address.user == nil && address.password == nil
    case .game(let game): return game.allowsDocument(address: address.absoluteString)
    case .video(let video):
      return video.allowsNavigation(url: address.absoluteString, subframe: subframe)
    }
  }

  public func resourceRules() throws -> String {
    guard let url, permits(url), let host = url.host else { throw SiteReadError.malformed }
    func rule(_ pattern: String, _ action: String) -> [String: [String: String]] {
      ["trigger": ["url-filter": pattern], "action": ["type": action]]
    }
    var rules = [rule(".*", "block")]
    switch self {
    case .confirmation:
      rules.append(
        rule(
          "^https://" + NSRegularExpression.escapedPattern(for: host) + "(:443)?/",
          "ignore-previous-rules"))
    case .game, .video:
      let label = "[a-z0-9]([a-z0-9-]*[a-z0-9])?"
      let prefix = "^https://(" + label + "\\.)+"
      rules.append(rule(prefix + "[a-z][a-z]+(:443)?/", "ignore-previous-rules"))
      for suffix in ["internal", "test", "invalid", "localhost", "local"] {
        rules.append(rule(prefix + suffix + "(:443)?/", "block"))
      }
    }
    return String(decoding: try JSONSerialization.data(withJSONObject: rules), as: UTF8.self)
  }
}
