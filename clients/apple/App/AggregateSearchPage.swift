import ParkCore
import SwiftUI

@MainActor struct AggregateSearchPage: View {
  let session: ProgressSession
  let locale: String
  let incognito: Bool
  let tr: (String) -> String
  let back: () -> Void
  var initialKeyword = ""
  var category: String? = nil
  var preferred: String? = nil
  var metadataOnly = false
  @State private var scope: SearchScope?
  @State private var query = ""
  @State private var term = ""
  @State private var revision = 0
  @State private var reload = 0
  @State private var active: AggregateSearch?
  @State private var rows: [SiteSearchSection] = []
  @State private var group: String?
  @State private var selected: (site: InstalledSite, item: SiteWork)?
  @State private var choosing = false
  @State private var excluded = Set<String>()
  @State private var saving = false
  @State private var status: String?

  var body: some View {
    Group {
      if let selected, let library = InstalledSources.library {
        if metadataOnly {
          ReferenceDetailPage(session: session, library: library, site: selected.site, id: selected.item.id, tr: tr) { self.selected = nil }
        } else { SiteDetails(session: session, library: library, site: selected.site, workId: selected.item.id,
                    preview: selected.item, locale: locale, incognito: incognito, tr: tr) {
          self.selected = nil
        } }
      } else {
        VStack(alignment: .leading) {
          HStack {
            Button(tr("返回")) {
              if group != nil { group = nil } else { back() }
            }
            if let group, let row = rows.first(where: { $0.id == group }) {
              Text(row.site.name ?? row.id).font(.headline)
            } else {
              TextField(tr("搜索"), text: $query).onSubmit(submit)
                .accessibilityIdentifier("aggregate-keyword")
              Button(tr("搜索"), action: submit).disabled(scope == nil)
              if category == nil { Button(tr("站点")) { excluded = scope?.excluded ?? []; choosing = true }.disabled(scope == nil) }
            }
          }.padding(.horizontal)
          if let status {
            Text(tr(status)).padding(.horizontal)
            if scope == nil { Button(tr("重试")) { reload += 1 } }
          } else if scope == nil { ProgressView() }
          List {
            ForEach(rows.filter { group == nil || $0.id == group }) { row in
              Section(header: Text(row.site.name ?? row.site.key)) {
                ForEach(row.items) { item in
                  if metadataOnly ? row.site.metadataDetail : row.site.detail { Button(item.title) { selected = (row.site, item) } }
                  else { Text(item.title) }
                }
                if row.busy { ProgressView() }
                else if let error = row.error {
                  Text(tr(siteMessage(error)))
                  Button(tr("重试")) { Task { await active?.more(site: row.id) } }
                } else if row.hasNext && group == nil && rows.count > 1 {
                  Button(tr("查看全部结果")) { group = row.id }
                    .accessibilityHint(row.site.name ?? row.id)
                    .accessibilityIdentifier("results:" + row.id)
                } else if row.hasNext {
                  Button(tr("加载更多")) { Task { await active?.more(site: row.id) } }
                } else if row.items.isEmpty { Text(tr("暂无内容")) }
              }
            }
            if scope != nil && !term.isEmpty && rows.isEmpty { Text(tr("暂无内容")) }
          }
        }
      }
    }.task(id: reload) {
      do {
        guard let library = InstalledSources.library else { throw SiteReadError.malformed }
        scope = try await session.searchScope(library, category: category, preferred: preferred, metadataOnly: metadataOnly)
        if !initialKeyword.isEmpty && term.isEmpty { query = initialKeyword; submit() }
        status = nil
      } catch { if !Task.isCancelled { status = "站点加载失败，请重试" } }
    }.task(id: revision) {
      guard let library = InstalledSources.library, let scope, !term.isEmpty else { return }
      let batch = AggregateSearch(session: session, library: library, keyword: term, scope: scope)
      active = batch
      await batch.start()
      for await snapshot in batch.updates {
        guard !Task.isCancelled, active === batch else { break }
        rows = snapshot
      }
      await batch.close()
      if active === batch { active = nil }
    }
    .sheet(isPresented: $choosing) {
      VStack(alignment: .leading) {
        Text(tr("站点")).font(.headline)
        List(scope?.sites ?? []) { site in
          Toggle(site.name ?? site.key, isOn: Binding(get: { !excluded.contains(site.key) }, set: { include in
            if include { excluded.remove(site.key) } else { excluded.insert(site.key) }
          }))
        }
        if let status { Text(tr(status)) }
        HStack {
          Button(tr("取消")) { choosing = false }
          Spacer()
          Button(tr("应用")) {
            guard !saving else { return }
            saving = true
            Task {
              defer { saving = false }
              do {
                guard let library = InstalledSources.library else { throw SiteReadError.malformed }
                try await session.setSearchExcluded(excluded)
                scope = try await session.searchScope(library)
                status = nil
                choosing = false
                rows = []
                revision += 1
              } catch { status = "保存失败，请重试" }
            }
          }
        }
      }.padding().disabled(saving).interactiveDismissDisabled(saving)
    }
  }

  private func submit() {
    guard scope != nil else { return }
    group = nil
    term = query.trimmingCharacters(in: .whitespacesAndNewlines)
    rows = []
    revision += 1
  }
}
