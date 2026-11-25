Any *.rs files have a hard line cap at 400 lines, if anything is over that, then split them up into multiple files.
Split up into more crates if it makes compile time faster and makes it easier for more swarm workers/subagents to work on.
