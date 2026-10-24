// Sync badges in Dolphin for DriveDeck: asks the app's tray, over the session bus, what
// state each file is in (docs/badges-design.md). No tray running = no badges.
#include <KOverlayIconPlugin>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QHash>
#include <QList>
#include <QSet>
#include <QTimer>
#include <QUrl>

namespace {
const QString Service = QStringLiteral("io.github.sweatshopking.DriveDeck.Badges");
const QString Path = QStringLiteral("/io/github/sweatshopking/DriveDeck/Badges");
const QString Iface = Service;
}

class Badges : public KOverlayIconPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.kde.overlayicon.drivedeck" FILE "badges.json")

public:
    Badges()
    {
        auto bus = QDBusConnection::sessionBus();
        bus.connect(Service, Path, Iface, QStringLiteral("Changed"), this, SLOT(changed()));
        bus.connect(QStringLiteral("org.freedesktop.DBus"), QStringLiteral("/org/freedesktop/DBus"),
                    QStringLiteral("org.freedesktop.DBus"), QStringLiteral("NameOwnerChanged"),
                    this, SLOT(ownerChanged(QString,QString,QString)));
    }

    QStringList getOverlays(const QUrl &item) override
    {
        if (!item.isLocalFile()) {
            return {};
        }
        const auto cached = m_seen.constFind(item);
        if (cached != m_seen.cend()) {
            return emblem(cached.value());
        }
        if (m_seen.size() >= MaxSeen && !m_seen.contains(item)) {
            // forget what was shown earliest; it's asked about again when shown again
            const QUrl oldest = m_order.takeFirst();
            m_seen.remove(oldest);
            m_pending.remove(oldest);
        }
        m_order.append(item);
        m_seen.insert(item, QString());
        m_pending.insert(item);
        schedulePending();
        // The answer arrives as overlaysChanged after a single batched request.
        return {};
    }

private Q_SLOTS:
    void flushPending()
    {
        m_flushScheduled = false;
        if (m_refreshing || m_pending.isEmpty()) {
            return;
        }
        QList<QUrl> urls;
        for (const QUrl &u : std::as_const(m_order)) {
            if (m_pending.remove(u)) {
                urls.append(u);
            }
        }
        request(urls);
    }

    void changed()
    {
        // ask again for everything shown so far in one asynchronous call
        refreshAll();
    }

    void ownerChanged(const QString &name, const QString &, const QString &owner)
    {
        if (name != Service) {
            return;
        }
        ++m_ownerGeneration;
        if (!owner.isEmpty()) {
            refreshAll();
            return;
        }
        m_refreshAgain = false;
        // Don't leave badges looking current after the tray exits. When it starts again, one
        // batched request below repopulates the cache.
        for (const QUrl &u : std::as_const(m_order)) {
            QString &state = m_seen[u];
            if (!state.isEmpty()) {
                state.clear();
                Q_EMIT overlaysChanged(u, QStringList{});
            }
        }
    }

private:
    void schedulePending()
    {
        if (m_flushScheduled) {
            return;
        }
        m_flushScheduled = true;
        // Calls from one directory read are collected into a single batch.
        QTimer::singleShot(25, this, &Badges::flushPending);
    }

    void refreshAll()
    {
        if (m_refreshing) {
            m_refreshAgain = true;
            return;
        }
        for (const QUrl &u : std::as_const(m_order)) {
            m_pending.remove(u);
        }
        request(m_order);
    }

    void request(const QList<QUrl> &urls)
    {
        if (urls.isEmpty()) {
            return;
        }
        if (m_refreshing) {
            m_refreshAgain = true;
            return;
        }
        m_refreshing = true;
        QStringList paths;
        paths.reserve(urls.size());
        for (const QUrl &u : urls) {
            paths << u.toLocalFile();
        }
        QDBusMessage m = QDBusMessage::createMethodCall(Service, Path, Iface, QStringLiteral("GetStatus"));
        m << paths;
        m.setAutoStartService(false);
        const int generation = m_ownerGeneration;
        auto *watcher = new QDBusPendingCallWatcher(QDBusConnection::sessionBus().asyncCall(m, 300), this);
        connect(watcher, &QDBusPendingCallWatcher::finished, this,
                [this, watcher, urls, generation]() {
                    const QDBusMessage reply = watcher->reply();
                    const QStringList states = reply.arguments().isEmpty()
                        ? QStringList()
                        : reply.arguments().first().toStringList();
                    if (generation == m_ownerGeneration
                        && reply.type() == QDBusMessage::ReplyMessage
                        && states.size() == urls.size()) {
                        for (int i = 0; i < urls.size(); ++i) {
                            auto cached = m_seen.find(urls.at(i));
                            if (cached == m_seen.end()) {
                                continue; // this path was evicted while the request was in flight
                            }
                            QString &was = cached.value();
                            if (states.at(i) != was) {
                                was = states.at(i);
                                Q_EMIT overlaysChanged(urls.at(i), emblem(was));
                            }
                        }
                    }
                    watcher->deleteLater();
                    m_refreshing = false;
                    if (m_refreshAgain) {
                        m_refreshAgain = false;
                        refreshAll();
                    } else {
                        flushPending();
                    }
                });
    }

    static QStringList emblem(const QString &state)
    {
        if (state.isEmpty()) {
            return {};
        }
        return {QStringLiteral("emblem-drivedeck-") + state};
    }

    static constexpr int MaxSeen = 5000;
    QHash<QUrl, QString> m_seen;
    QList<QUrl> m_order; // m_seen's keys, earliest shown first
    QSet<QUrl> m_pending;
    bool m_flushScheduled = false;
    bool m_refreshing = false;
    bool m_refreshAgain = false;
    int m_ownerGeneration = 0;
};

#include "badges.moc"
