#include "../src/commandarguments.h"
#include <cassert>
#include <cmath>

int main()
{
    QLocale::setDefault(QLocale(QLocale::Italian));
    assert(commandArgument(QVariant(48.8584)) == QStringLiteral("48.8584"));
    assert(commandArgument(QVariant(qlonglong(1234567890123456789LL)))
           == QStringLiteral("1234567890123456789"));
    assert(commandArgument(QVariant(std::numeric_limits<qulonglong>::max()))
           == QStringLiteral("18446744073709551615"));
    assert(commandArgument(QVariant(std::numeric_limits<qlonglong>::min()))
           == QStringLiteral("-9223372036854775808"));
    assert(commandArgument(QVariant(QStringLiteral("21C"))) == QStringLiteral("21C"));
    for (double value : {std::nextafter(1.0, 2.0), 48.8584, -2.2945, 1e-100,
                         std::numeric_limits<double>::denorm_min(),
                         std::numeric_limits<double>::max()}) {
        bool ok = false;
        const double parsed = QLocale::c().toDouble(commandArgument(QVariant(value)), &ok);
        assert(ok && parsed == value);
    }
}
