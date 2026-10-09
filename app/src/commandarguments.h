#ifndef COMMANDARGUMENTS_H
#define COMMANDARGUMENTS_H

#include <QLocale>
#include <QVariant>
#include <limits>

// Keep integer precision and use locale-independent, round-trippable doubles.
// Sailfish's Qt 5.6 predates QLocale::FloatingPointShortest: find the first
// significant-digit precision that parses back to the original double.
inline QString commandArgument(const QVariant &arg)
{
    if (arg.type() == QVariant::LongLong || arg.type() == QVariant::Int)
        return QString::number(arg.toLongLong());
    if (arg.type() == QVariant::ULongLong || arg.type() == QVariant::UInt)
        return QString::number(arg.toULongLong());
    if (arg.type() != QVariant::Double)
        return arg.toString();

    const QLocale cLocale = QLocale::c();
    const double value = arg.toDouble();
    QString text;
    for (int precision = 1; precision <= std::numeric_limits<double>::max_digits10; ++precision) {
        text = cLocale.toString(value, 'g', precision);
        bool ok = false;
        if (cLocale.toDouble(text, &ok) == value && ok)
            break;
    }
    return text;
}

#endif
