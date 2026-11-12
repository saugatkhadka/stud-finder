module REST
  # No Widget model: nothing inferred.
  class WidgetSerializer < ActiveModel::Serializer
  end

  # TagParser exists, but in lib/: not a model.
  class TagParserSerializer < ActiveModel::Serializer
  end
end
