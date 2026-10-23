# Text that players sent (reports, upload names) made safe to print in a terminal: control
# characters (escape sequences can hide or fake text, plant links or set the clipboard) and
# Unicode's direction overrides (they reorder what is shown) are dropped; new lines and tabs
# stay. reports.sh and library.sh print everything through this.
import re

UNSAFE = re.compile("[\x00-\x08\x0b-\x1f\x7f-\x9f؜‎‏‪-‮⁦-⁩]")


def clean(value):
    return UNSAFE.sub("", "" if value is None else str(value))
