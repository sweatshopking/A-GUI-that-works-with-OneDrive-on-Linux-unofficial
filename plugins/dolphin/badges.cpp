// Sync badges in Dolphin for DriveDeck: asks the app's tray, over the session bus, what
// state each file is in (docs/badges-design.md). No tray running = no badges.
#include <KOverlayIconPlugin>
#include <QDBusConnection>
#include <QDBusMessage>
#include <QHash>
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
        const QString state = ask(item.toLocalFile());
        m_seen.insert(item, state);
        return emblem(state);
    }

private Q_SLOTS:
    void changed()
    {
        // ask again for everything shown so far; tell Dolphin only about what changed
        for (auto it = m_seen.begin(); it != m_seen.end(); ++it) {
            const QString now = ask(it.key().toLocalFile());
            if (now != it.value()) {
                it.value() = now;
                Q_EMIT overlaysChanged(it.key(), emblem(now));
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

    static QString ask(const QString &path)
    {
        QDBusMessage m = QDBusMessage::createMethodCall(Service, Path, Iface, QStringLiteral("GetStatus"));
        m << QStringList{path};
        m.setAutoStartService(false);
        const QDBusMessage r = QDBusConnection::sessionBus().call(m, QDBus::Block, 300);
        if (r.type() != QDBusMessage::ReplyMessage || r.arguments().isEmpty()) {
            return {};
        }
        const QStringList s = r.arguments().first().toStringList();
        return s.isEmpty() ? QString() : s.first();
    }

    QHash<QUrl, QString> m_seen;
};

#include "badges.moc"
