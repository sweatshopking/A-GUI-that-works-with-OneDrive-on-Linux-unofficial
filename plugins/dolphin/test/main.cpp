// Headless check of the Dolphin badge plugin: load it the way Dolphin does and ask for badges.
#include <KOverlayIconPlugin>
#include <QPluginLoader>
#include <KPluginMetaData>
#include <QCoreApplication>
#include <QTimer>
#include <QUrl>
#include <cstdio>

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
    }
    return bad;
}
