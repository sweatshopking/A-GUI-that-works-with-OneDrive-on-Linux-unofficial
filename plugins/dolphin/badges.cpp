// Sync badges in Dolphin for DriveDeck: asks the app's tray, over the session bus, what
// state each file is in (docs/badges-design.md). No tray running = no badges.
#include <KOverlayIconPlugin>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QHash>
#include <QList>
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
        QDBusConnection::sessionBus().connect(Service, Path, Iface, QStringLiteral("Changed"), this,
                                              SLOT(changed()));
    }

    QStringList getOverlays(const QUrl &item) override
    {
        if (!item.isLocalFile()) {
            return {};
        }
        const QString state = ask({item.toLocalFile()}).value(0);
        if (m_seen.size() >= MaxSeen && !m_seen.contains(item)) {
            // forget what was shown earliest; it's asked about again when shown again
            m_seen.remove(m_order.takeFirst());
        }
        if (!m_seen.contains(item)) {
            m_order.append(item);
        }
        m_seen.insert(item, state);
        return emblem(state);
    }

private Q_SLOTS:
    void changed()
    {
        // ask again for everything shown so far, in one call; tell Dolphin only what changed
        QStringList paths;
        paths.reserve(m_order.size());
        for (const QUrl &u : std::as_const(m_order)) {
            paths << u.toLocalFile();
        }
        const QStringList now = ask(paths);
        if (now.size() != m_order.size()) {
            return; // no tray (or an odd answer): keep what's shown
        }
        for (int i = 0; i < m_order.size(); ++i) {
            QString &was = m_seen[m_order.at(i)];
            if (now.at(i) != was) {
                was = now.at(i);
                Q_EMIT overlaysChanged(m_order.at(i), emblem(was));
            }
        }
    }

private:
    static QStringList emblem(const QString &state)
    {
        if (state.isEmpty()) {
            return {};
        }
        return {QStringLiteral("emblem-drivedeck-") + state};
    }

    // one answer per path, in the same order; nothing when no tray answers
    static QStringList ask(const QStringList &paths)
    {
        QDBusMessage m = QDBusMessage::createMethodCall(Service, Path, Iface, QStringLiteral("GetStatus"));
        m << paths;
        m.setAutoStartService(false);
        const QDBusMessage r = QDBusConnection::sessionBus().call(m, QDBus::Block, 300);
        if (r.type() != QDBusMessage::ReplyMessage || r.arguments().isEmpty()) {
            return {};
        }
        return r.arguments().first().toStringList();
    }

    static constexpr int MaxSeen = 5000;
    QHash<QUrl, QString> m_seen;
    QList<QUrl> m_order; // m_seen's keys, earliest shown first
};

#include "badges.moc"
