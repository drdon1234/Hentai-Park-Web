import Foundation

public enum GameKey: String, CaseIterable, Sendable {
  case up = "ArrowUp"
  case left = "ArrowLeft"
  case down = "ArrowDown"
  case right = "ArrowRight"
  case z = "KeyZ"
  case enter = "Enter"
  case space = "Space"
  case x = "KeyX"
  case escape = "Escape"
  case control = "ControlLeft"

  public var label: String {
    switch self {
    case .up: return "上"
    case .left: return "左"
    case .down: return "下"
    case .right: return "右"
    case .z: return "Z"
    case .x: return "X"
    case .escape: return "Esc"
    case .control: return "Ctrl"
    default: return rawValue
    }
  }
  private var key: String {
    switch self {
    case .z: return "z"
    case .x: return "x"
    case .space: return " "
    case .control: return "Control"
    default: return rawValue
    }
  }
  private var number: Int {
    switch self {
    case .up: return 38
    case .left: return 37
    case .down: return 40
    case .right: return 39
    case .z: return 90
    case .enter: return 13
    case .space: return 32
    case .x: return 88
    case .escape: return 27
    case .control: return 17
    }
  }

  /// 仅派发用户操作的固定虚拟键，目标为当前游戏主文档。
  fileprivate func script(down: Bool, ctrl: Bool) -> String {
    """
    (() => {
      const event = new KeyboardEvent('\(down ? "keydown" : "keyup")', {
        key: '\(key)', code: '\(rawValue)', ctrlKey: \(ctrl), bubbles: true, cancelable: true
      });
      Object.defineProperties(event, {keyCode: {get: () => \(number)}, which: {get: () => \(number)}});
      (document.activeElement || document.body || document).dispatchEvent(event);
    })();
    """
  }
}

public struct GameKeyboard {
  private var held = Set<GameKey>()
  public init() {}

  public mutating func press(_ key: GameKey, down: Bool) -> String? {
    let changed = down ? held.insert(key).inserted : held.remove(key) != nil
    return changed ? key.script(down: down, ctrl: held.contains(.control)) : nil
  }

  public mutating func releaseAll() -> [String] {
    GameKey.allCases.compactMap { press($0, down: false) }
  }
}
