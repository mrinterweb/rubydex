require "parent"

class Child < Parent
  alias_method :nickname, :parent_method

  def child_method
    self.parent_method
  end

  def self.solo
    "solo"
  end
end
