// Headless check of the Dolphin badge plugin: load it the way Dolphin does and ask for badges.
#include <KOverlayIconPlugin>
#include <QFile>
#include <QPluginLoader>
#include <KPluginMetaData>
#include <QCoreApplication>
#include <QTimer>
#include <QUrl>
#include <cstdio>

static int statusCalls()
{
    QFile f(QStringLiteral("/tmp/badges-call-count"));
    if (!f.open(QIODevice::ReadOnly)) {
        return -1;
    }
    return f.readAll().trimmed().toInt();
}

int main(int argc, char **argv)
{
    QCoreApplication app(argc, argv);
    int bad = 0;
    for (const KPluginMetaData &md : KPluginMetaData::findPlugins(QStringLiteral("kf6/overlayicon"), {}, KPluginMetaData::AllowEmptyMetaData)) {
        // the way Dolphin's KFileItemModelRolesUpdater loads overlay plugins
        QPluginLoader loader(md.fileName());
        auto *plugin = qobject_cast<KOverlayIconPlugin *>(loader.instance());
        if (!plugin) {
            printf("LOAD FAIL %s: %s\n", qPrintable(md.fileName()), qPrintable(loader.errorString()));
            return 1;
        }
        const struct { const char *path, *want; } cases[] = {
            {"/t/OneDrive/a.txt", "emblem-drivedeck-synced"},
            {"/t/OneDrive/v (syncing).mkv", "emblem-drivedeck-syncing"},
            {"/t/OneDrive/r (error).xlsx", "emblem-drivedeck-error"},
            {"/t/elsewhere/x.txt", ""},
        };
        for (const auto &c : cases) {
            const QStringList got = plugin->getOverlays(QUrl::fromLocalFile(QString::fromUtf8(c.path)));
            const QString g = got.value(0);
            const bool ok = g == QString::fromUtf8(c.want);
            bad += !ok;
            printf("%s %s -> '%s'\n", ok ? "ok  " : "FAIL", c.path, qPrintable(g));
        }
        const auto path = QUrl::fromLocalFile(QStringLiteral("/t/OneDrive/a.txt"));
        const QStringList again = plugin->getOverlays(path);
        const bool cached = again.value(0) == QStringLiteral("emblem-drivedeck-synced")
            && statusCalls() == 4;
        bad += !cached;
        printf("%s repeated path uses cached status\n", cached ? "ok  " : "FAIL");
        // "Changed": the plugin asks again (in one call) and reports only what changed
        int reported = 0;
        QString aNow;
        QObject::connect(plugin, &KOverlayIconPlugin::overlaysChanged, [&](const QUrl &u, const QStringList &o) {
            ++reported;
            if (u.path().endsWith(QStringLiteral("/a.txt"))) {
                aNow = o.value(0);
            }
        });
        FILE *f = fopen("/tmp/badges-flip", "w");
        if (f) {
            fclose(f);
        }
        QTimer::singleShot(3000, &app, &QCoreApplication::quit);
        app.exec();
        const bool ok = reported == 1 && aNow == QStringLiteral("emblem-drivedeck-error") && statusCalls() == 5;
        bad += !ok;
        printf("%s Changed -> %d update(s), a.txt now '%s'\n", ok ? "ok  " : "FAIL", reported, qPrintable(aNow));
        remove("/tmp/badges-flip");
    }
    return bad;
}
