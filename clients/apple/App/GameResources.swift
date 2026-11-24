import ParkCore
import SwiftUI
#if os(macOS)
import AppKit
#else
import UIKit
#endif

@MainActor private func copyGameField(_ value: String) {
  #if os(macOS)
  NSPasteboard.general.clearContents()
  NSPasteboard.general.setString(value, forType: .string)
  #else
  UIPasteboard.general.string = value
  #endif
}

@MainActor struct GameResourcePage: View {
  let session: ProgressSession
  let library: SourceLibrary
  let work: LibraryWork
  let tr: (String) -> String
  let back: () -> Void
  @EnvironmentObject private var queue: DownloadQueue
  @State private var list: GameResourceList?
  @State private var attempt = 0
  @State private var current: Choice?
  @State private var consent: Choice?
  @State private var resolved: ResolvedGameResource?
  @State private var failure: String?
  @State private var busy = false
  @State private var retry = 0
  @State private var copied: String?

  private struct Choice: Equatable {
    let resource: String
    let channel: SiteGameResource.Channel
    static func == (a: Self, b: Self) -> Bool { a.resource == b.resource && a.channel.id == b.channel.id }
  }
  private func select(_ value: Choice) {
    copied = nil
    resolved = nil
    failure = nil
    current = value
    retry += 1
  }
  var body: some View {
    VStack(alignment: .leading) {
      Button(tr("返回"), action: back)
      List {
        Text(work.title).font(.headline)
        if let list {
          if list.entries.isEmpty { Text(tr("暂无游戏资源")) }
          ForEach(list.entries) { resource in
            Section(header: Text(resource.title.isEmpty ? resource.id : resource.title)) {
              let metadata = ([resource.version,resource.size] + resource.platforms + resource.languages).filter { !$0.isEmpty }
              if !metadata.isEmpty { Text(metadata.joined(separator: " · ")) }
              Text(tr(resource.testStatus == "tested" ? "原站标记已测试" : resource.testStatus == "untested" ? "未测试" : "未提供测试状态"))
              if !resource.notes.isEmpty { Text(resource.notes).textSelection(.enabled) }
              if resource.expired { Text(tr("已失效")) }
              ForEach(resource.channels) { channel in
                Button(channel.label.isEmpty ? channel.id : channel.label) {
                  let choice = Choice(resource:resource.id,channel:channel)
                  if channel.requiresReply { consent = choice } else { select(choice) }
                }.disabled(resource.expired || busy)
                if current == Choice(resource:resource.id,channel:channel), let resolved {
                  fields(resolved, resource:resource.id, channel:channel.id)
                }
              }
            }
          }
          GameQueueRows(session:session, source:work.source, work:work.workId, tr:tr)
        }
        if busy || list == nil && failure == nil { ProgressView() }
        if let failure {
          Text(tr(failure))
          Button(tr("重试")) {
            if let current {
              if current.channel.requiresReply { consent = current } else { select(current) }
            } else { attempt += 1 }
          }
        }
      }
    }
    .task(id: attempt) {
      failure = nil
      do {
        let result = try await session.gameResources(library,source:work.source,work:work.workId)
        try Task.checkCancellation()
        list = result
      } catch { if !Task.isCancelled { failure = "加载失败" } }
    }
    .task(id: retry) {
      guard let list, let current else { return }
      busy = true
      do {
        let result = try await session.resolveGameChannel(list,resource:current.resource,channel:current.channel,
          confirmedReply:current.channel.requiresReply ? current.channel.replyText : nil)
        try Task.checkCancellation()
        resolved = result
      } catch { if !Task.isCancelled { failure = "加载失败" } }
      if !Task.isCancelled { busy = false }
    }
    .alert(tr("回复解锁并获取链接"), isPresented: Binding(get:{consent != nil}, set:{if !$0 { consent=nil }}), presenting: consent) { choice in
      Button(tr("取消"), role:.cancel) {}
      Button(tr("回复并获取链接")) { select(choice) }
    } message: { choice in Text(choice.channel.replyText) }
  }

  @ViewBuilder private func fields(_ result: ResolvedGameResource, resource: String, channel: String) -> some View {
    let data = result.fields
    if data.confirmRequired == true {
      Text(tr("请在站点页面中完成确认后继续"))
      if let original=data.original, let url=URL(string:original) { Link(tr("去确认"), destination:url) }
    } else {
      ForEach(Array((data.urls ?? []).enumerated()), id: \.offset) { index, value in
        VStack(alignment:.leading) {
          if let url=URL(string:value) { Link(tr("打开链接"), destination:url) }
          Text(value).textSelection(.enabled)
          copyButton("复制",value)
          let codes=data.extractionCodes ?? []
          let code=codes.indices.contains(index) && !codes[index].isEmpty ? codes[index] : data.extractionCode ?? ""
          if !code.isEmpty { Text(tr("提取码")+" · "+code);copyButton("复制提取码",code) }
        }
      }
      if let password=data.archivePassword, !password.isEmpty {
        Text(tr("解压密码")+" · "+password)
        copyButton("复制解压密码",password)
      }
      if data.direct == true {
        Button(tr("直链下载")) {
          busy=true
          Task {
            defer { busy=false }
            do {
              try await queue.enqueueGame(result,key:[work.source,work.workId,resource,channel],title:work.title,fallback:session)
              resolved=nil
            } catch { failure="下载失败" }
          }
        }.disabled(busy)
      }
    }
  }
  private func copyButton(_ key:String,_ value:String) -> some View {
    Button(tr(copied==value ? "已复制" : key)) { copyGameField(value);copied=value }.buttonStyle(.borderless)
  }
}

@MainActor struct GameQueueRows: View {
  let session: ProgressSession
  var source: String?
  var work: String?
  var media: String?
  var status: Int32?
  var showEmpty = false
  let tr: (String) -> String
  @EnvironmentObject private var queue: DownloadQueue
  @Environment(\.scenePhase) private var phase
  @State private var records: [GameDownloadRecord]?
  @State private var failed = false
  @State private var retry = 0
  private var visible: [GameDownloadRecord] {
    (records ?? []).filter { (source==nil || $0.item.source==source) && (work==nil || $0.item.id==work) &&
      (media==nil || media=="game") && (status==nil || status==$0.statusCode) }
  }
  private struct Refresh: Equatable { let revision,retry:Int; let active:Bool }
  var body: some View {
    Section {
      if records == nil && !failed { ProgressView() }
      ForEach(visible) { row in GameDownloadRow(session:session,row:row,tr:tr) }
      if showEmpty && records != nil && visible.isEmpty && !failed { Text(tr("没有下载任务")) }
      if failed {
        Text(tr("加载失败")); Button(tr("重试")) { retry += 1 }
      }
    }.task(id:Refresh(revision:queue.revision,retry:retry,active:phase == .active)) {
      guard phase == .active else { return }
      do {
        while true {
          let next=try await session.gameDownloads()
          try Task.checkCancellation()
          records=next;failed=false
          if !next.contains(where: { $0.active }) { break }
          try await Task.sleep(nanoseconds:750_000_000)
        }
      } catch { if !Task.isCancelled { failed=true } }
    }
  }
}

@MainActor private struct GameDownloadRow: View {
  let session: ProgressSession
  let row: GameDownloadRecord
  let tr: (String) -> String
  @EnvironmentObject private var queue: DownloadQueue
  @State private var busy=false
  @State private var failed=false
  @State private var copied=false
  @State private var exporting=false
  var body: some View {
    VStack(alignment:.leading,spacing:6) {
      Text(row.item.title ?? row.item.id).font(.headline)
      Text(row.filename).textSelection(.enabled)
      Text(tr(row.missing ? "本地文件已被删除" : ["queued":"等待下载","running":"下载中","paused":"已暂停","failed":"下载失败","done":"下载完成"][row.status] ?? "下载失败"))
      if row.active, let total=row.total, total>0 { ProgressView(value:Double(min(total,row.bytes ?? 0)),total:Double(total)) }
      if let code=row.extractionCode, !code.isEmpty { Text(tr("提取码")+" · "+code).textSelection(.enabled) }
      if let password=row.archivePassword, !password.isEmpty { Text(tr("解压密码")+" · "+password).textSelection(.enabled) }
      if row.active { Button(tr("取消下载")) { change("pause") } }
      else {
        if row.status=="done", let file=row.file {
          Button(tr(copied ? "已复制" : "复制文件路径")) { copyGameField(file);copied=true }
          Button(tr("导出游戏文件")) { export(URL(fileURLWithPath:file)) }
          #if os(iOS)
            .sheet(isPresented:$exporting) { GameExportPicker(url:URL(fileURLWithPath:file)) }
          #endif
        } else { Button(tr("重新下载")) { change("resume") } }
        Button(tr("移除任务（保留文件）")) { change("remove") }
      }
      if failed { Text(tr("操作失败，请重试")) }
    }.buttonStyle(.borderless).disabled(busy)
  }
  private func change(_ action:String) {
    busy=true
    Task {
      defer { busy=false }
      do { try await queue.changeGame(row,action:action,fallback:session);failed=false } catch { failed=true }
    }
  }
  private func export(_ url:URL) {
    #if os(macOS)
    guard let window=NSApp.keyWindow else {failed=true;return}
    let panel=NSSavePanel()
    panel.nameFieldStringValue=row.filename
    panel.beginSheetModal(for:window) { response in
      guard response == .OK, let destination=panel.url else { return }
      busy=true
      Task {
        defer { busy=false }
        do {
          try await Task.detached {
            if url.standardizedFileURL == destination.standardizedFileURL { return }
            let files=FileManager.default
            let temp=try files.url(for:.itemReplacementDirectory,in:.userDomainMask,appropriateFor:destination,create:true)
            defer { try? files.removeItem(at:temp) }
            let copy=temp.appendingPathComponent("export")
            try files.copyItem(at:url,to:copy)
            if files.fileExists(atPath:destination.path) { _=try files.replaceItemAt(destination,withItemAt:copy) }
            else { try files.moveItem(at:copy,to:destination) }
          }.value
          failed=false
        } catch { failed=true }
      }
    }
    #else
    exporting=true
    #endif
  }
}
#if os(iOS)
private struct GameExportPicker: UIViewControllerRepresentable {
  let url:URL
  func makeUIViewController(context:Context) -> UIDocumentPickerViewController {
    UIDocumentPickerViewController(forExporting:[url],asCopy:true)
  }
  func updateUIViewController(_ controller:UIDocumentPickerViewController,context:Context) {}
}
#endif
