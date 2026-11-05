-- The chunk receives the existing protocol object as its first argument.
local input = ...
if input.event == "before_tool" then
    if input.data.arguments.number < 0 then
        return {decision = "deny", reason = "The demo only accepts non-negative numbers"}
    end
    return {}
end
if input.event == "tool" then
    return {content = tostring(input.arguments.number * 2), is_error = false}
end
return {}
